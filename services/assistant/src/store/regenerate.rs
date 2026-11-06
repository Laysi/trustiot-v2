//! Regenerating the last answer (Laysi/trustiot-v2#649; docs/architecture/ai-assistant.md,
//! 「對話歷史只往後加」 rule 4): the history's last round is discarded and its question asked
//! again, in the one transaction that also records the new question's run, so the round is
//! never discarded without the question starting again, and never while it may not start.
//!
//! [`admit_regeneration`] is [`super::admit_question`]'s counterpart: the same locks and
//! refusals, then the last round's checks, its question's text, the user's daily starts, the
//! discard, and the history's size as the new question continues it.
//!
//! **The request names the answer it regenerates** (the user's decision of 2026-11-05): the
//! position the owner's conversation gave it (`ConversationDetail`'s `messages[].seq`). Under
//! the conversation's lock, a name that is not the history's last answer is refused
//! ([`StoreError::StaleAnswer`]) before anything is discarded. Without it a retry would peel
//! one more round each time: after a regeneration's commit its round is gone whatever happens
//! next (the delegation refused, a lost response, a stopped run), so the same request sent
//! again, a double submit or a stale tab would discard the round before. With it each of those
//! is a 409 that changes nothing. The caller then takes the replica's seat and memory
//! for [`Regeneration::admitted`] and calls [`super::start_question`] in the same transaction;
//! any refusal before the commit rolls the discard back with everything else.
//!
//! **A copied round is not regenerated** (the user's decision of 2026-11-04). A copy of a
//! shared conversation (Laysi/trustiot-v2#650, `Agent::copied_conversation`) stores its last
//! copied round as the question, a note ([`EntryKind::Note`], `prompt::COPIED_ROUNDS`), and the
//! answer; the note tells the model that every copied round before it came from another member
//! and carries no instructions (the #650 security review's L1). Discarding that round would
//! discard the note too, so such a round is refused ([`StoreError::CopiedRound`]). The rule is
//! structural, never the note's text (Laysi/trustiot-v2#1072 changes the text): a round whose
//! question is followed directly by a note is a copied one. `Agent::ask` does not store a note
//! there: its only note, the budget's wind-down, goes before the model turn it leads to once
//! the question spent `budget.wind_down_percent` of its budget, which only a model call spends,
//! so the first turn (a step and its tool results) comes between the question and the note. The
//! one exception would be a budget whose wind-down point rounds to 0 tokens (under 100 tokens in
//! all), far smaller than any call's input, so no call is sent and nothing is stored. Every
//! copied round is therefore refused (no false negative); were an ordinary round ever stored
//! with a note right after its question, it would be refused too, wrongly but harmlessly: a
//! copy's note is never discarded.

use service_kit::tenant::TenantTransaction;
use sqlx::Row;
use uuid::Uuid;

use super::runs::{admit_locked, measure};
use super::{
    Admitted, INSERT_DISCARD, SELECT_MESSAGE, StoreError, count_starts, is_copied_round,
    raw_message,
};
use crate::history::{EntryKind, Principal};
use crate::model::{Format, NativeMessage};

/// `$1` tenant, `$2` conversation: the history's last round as [`super::load`] returns it, its
/// position and kind each, without the bodies: the messages that no discarded round holds, from
/// the last question on, in order. None when the history has no question.
const LAST_ROUND: &str = "WITH visible AS (
    SELECT m.seq, m.kind
    FROM assistant.messages m
    WHERE m.tenant_id = $1::uuid AND m.conversation_id = $2::uuid
        AND NOT EXISTS (
            SELECT 1 FROM assistant.discarded_rounds r
            WHERE r.tenant_id = m.tenant_id AND r.conversation_id = m.conversation_id
                AND m.seq BETWEEN r.from_seq AND r.to_seq))
SELECT seq, kind FROM visible
WHERE seq >= (SELECT max(seq) FROM visible WHERE kind = 'question')
ORDER BY seq";

/// A regeneration [`admit_regeneration`] let through: its last round is discarded in the
/// transaction, which then goes on to [`super::start_question`] with [`Self::admitted`].
#[derive(Debug)]
pub struct Regeneration {
    /// The history that remains, measured after the discard, for the caller's memory budget
    /// and [`super::start_question`].
    pub admitted: Admitted,
    /// The discarded round's question, to ask again.
    pub question: String,
    /// How many messages the discarded round held (the question, the steps, tool results and
    /// notes, the answer), for the log.
    pub discarded_messages: usize,
}

/// Lets the regeneration of the last answer in `principal`'s conversation `conversation`
/// through, or refuses it, in `tx`. `answer` is the position of the answer the caller
/// regenerates, as [`super::Loaded::positions`] gives it. In this order, every refusal before
/// anything is written (the caller rolls `tx` back on any error):
///
/// 1. The locks and refusals of [`super::admit_question`]'s steps 1 to 4: the user's start
///    lock, the conversation's row lock, a running question, the user's running limit (a
///    regeneration counts as one of the user's running questions).
/// 2. The last round of the history as [`super::load`] returns it (the last question and every
///    message after it, discarded rounds left out) must be an answered round asked in this
///    conversation: it ends with an answer, that answer is `answer`, and it is not a copied
///    round (the module documentation).
/// 3. The question's text, read from its stored message by [`NativeMessage::question_text`]:
///    only the form the service writes a question in.
/// 4. The user's starts in the window ([`super::MAX_STARTS_PER_DAY`]): a regeneration is one,
///    counted by the discarded round's `created_at`. The start lock taken in step 1 makes the
///    count exact.
/// 5. The round is discarded, from its question to its last message.
/// 6. The history that remains is measured ([`measure`]): one of
///    [`super::MAX_HISTORY_BYTES`] or more refuses the regeneration, and the discard is rolled
///    back with it. The limit is the history the new answer continues, so a conversation whose
///    last answer pushed it over the limit can regenerate that answer when the rest fits.
///
/// # Errors
///
/// [`StoreError::NotStored`] when the conversation is not theirs or deleted;
/// [`StoreError::AlreadyRunning`]; [`StoreError::RunLimited`];
/// [`StoreError::NothingToRegenerate`] when the last round has no answer, the history has no
/// round, or the question is not in the service's form; [`StoreError::StaleAnswer`] when
/// `answer` is not the last round's answer; [`StoreError::CopiedRound`];
/// [`StoreError::StartLimited`]; [`StoreError::HistoryFull`]; [`StoreError::Corrupt`] for a row that does not read back;
/// [`StoreError::WrongTenant`] or a database failure.
pub async fn admit_regeneration(
    tx: &mut TenantTransaction,
    principal: Principal,
    conversation: Uuid,
    answer: u32,
    max_running: u32,
) -> Result<Regeneration, StoreError> {
    admit_locked(tx, principal, conversation, max_running).await?;
    let tenant_text = principal.tenant_id.to_string();
    let conversation_text = conversation.to_string();
    let rows = sqlx::query(LAST_ROUND)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .fetch_all(&mut **tx)
        .await?;
    let mut round = Vec::with_capacity(rows.len());
    for row in &rows {
        let kind = EntryKind::from_name(&row.try_get::<String, _>("kind")?)
            .ok_or(StoreError::Corrupt("a message's kind"))?;
        round.push((row.try_get::<i32, _>("seq")?, kind));
    }
    let (from_seq, to_seq) = match (round.first(), round.last()) {
        (Some(&(from, EntryKind::Question)), Some(&(to, EntryKind::Answer))) => (from, to),
        _ => return Err(StoreError::NothingToRegenerate),
    };
    if u32::try_from(to_seq).ok() != Some(answer) {
        return Err(StoreError::StaleAnswer);
    }
    let kinds: Vec<EntryKind> = round.iter().map(|(_, kind)| *kind).collect();
    if is_copied_round(&kinds) {
        return Err(StoreError::CopiedRound);
    }
    let question = sqlx::query(SELECT_MESSAGE)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .bind(from_seq)
        .fetch_one(&mut **tx)
        .await?;
    let format = Format::from_name(&question.try_get::<String, _>("format")?)
        .ok_or(StoreError::Corrupt("a message's format"))?;
    let message = NativeMessage::new(format, raw_message(question.try_get("body")?)?);
    let question = message
        .question_text()
        .ok_or(StoreError::NothingToRegenerate)?;
    let starts = count_starts(
        tx,
        &tenant_text,
        &conversation_text,
        &principal.user_id.to_string(),
    )
    .await?;
    if starts.at_limit() {
        return Err(starts.limited());
    }
    sqlx::query(INSERT_DISCARD)
        .bind(&tenant_text)
        .bind(&conversation_text)
        .bind(from_seq)
        .bind(to_seq)
        .execute(&mut **tx)
        .await?;
    let admitted = measure(tx, principal, conversation).await?;
    Ok(Regeneration {
        admitted,
        question,
        discarded_messages: round.len(),
    })
}
