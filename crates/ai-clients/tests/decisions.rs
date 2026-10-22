//! OpenAI Decisions: predicate and score requests and answers from the official documentation
//! (sources in `tests/fixtures/README.md`), malformed answers, request checks, base64 images,
//! usage, and the client against a loopback server.

mod support;

use std::time::Duration;

use ai_clients::decisions::{
    Answer, DecisionInput, DecisionRequest, DecisionResponse, DecisionsClient, InputMessage,
    InputPart, InputRole, Question, ScoreLevel,
};
use ai_clients::{Error, ErrorClass, ReportedUsage, TimeoutKind, TokenUsage};
use base64::Engine as _;
use serde_json::{Value, json};
use support::{Server, Step, endpoint, fixture, fixture_json, http_response};

fn request(name: &str) -> DecisionRequest {
    serde_json::from_value(fixture_json(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn parse(value: &Value) -> Result<DecisionResponse, Error> {
    DecisionResponse::parse(&serde_json::to_vec(value).expect("JSON serializes"))
}

#[test]
fn predicate_and_score_answers_parse_and_fit_their_questions() {
    let cases = [
        (
            "decisions/predicate.request.json",
            "decisions/predicate.response.json",
        ),
        (
            "decisions/score.request.json",
            "decisions/score.response.json",
        ),
        (
            "decisions/router.request.json",
            "decisions/router.response.json",
        ),
        (
            "decisions/router.request.json",
            "decisions/router_refusal.response.json",
        ),
    ];
    for (request_name, response_name) in cases {
        let request = request(request_name);
        request.validate().unwrap();
        // Requests serialize back to what the documentation shows.
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            fixture_json(request_name)
        );
        let response = DecisionResponse::parse(fixture(response_name).as_bytes()).unwrap();
        response
            .validate_against(&request)
            .unwrap_or_else(|error| panic!("{response_name}: {error}"));
    }

    let predicate =
        DecisionResponse::parse(fixture("decisions/predicate.response.json").as_bytes()).unwrap();
    assert_eq!(
        predicate.answer("damaged"),
        Some(&Answer::Predicate {
            name: Some("damaged".into()),
            probability: 0.95
        })
    );

    let score =
        DecisionResponse::parse(fixture("decisions/score.response.json").as_bytes()).unwrap();
    let Some(Answer::Score {
        score: value,
        probabilities,
        confidence,
        ..
    }) = score.answer("severity")
    else {
        panic!("severity is a score answer");
    };
    assert!((value - 1.1).abs() < 1e-9);
    assert!((confidence - 0.55).abs() < 1e-9);
    let labels: Vec<&str> = probabilities
        .iter()
        .map(|level| level.label.as_str())
        .collect();
    assert_eq!(
        labels,
        ["Cosmetic", "Workaround available", "Fully blocked"]
    );

    let refusal =
        DecisionResponse::parse(fixture("decisions/router_refusal.response.json").as_bytes())
            .unwrap();
    assert_eq!(
        refusal.answer("simple"),
        Some(&Answer::Refusal {
            name: Some("simple".into())
        })
    );
}

#[test]
fn decisions_usage_bills_every_input_token_at_the_input_rate() {
    let response =
        DecisionResponse::parse(fixture("decisions/router.response.json").as_bytes()).unwrap();
    let usage = response.usage.unwrap();
    assert_eq!(
        usage.tokens(),
        TokenUsage {
            input: 187,
            ..TokenUsage::default()
        }
    );
    assert_eq!(usage.input_tokens_details.unwrap().cached_tokens, Some(64));
}

#[test]
fn malformed_answers_are_errors() {
    let request = request("decisions/router.request.json");
    let good = fixture_json("decisions/router.response.json");
    type Mutation = fn(&mut Value);
    let cases: [(&str, Mutation); 17] = [
        ("level probabilities that do not sum to 1", |v| {
            v["answers"][1]["probabilities"][2]["probability"] = json!(0.5);
        }),
        ("a score that is not the mean level", |v| {
            v["answers"][1]["score"] = json!(1.6);
        }),
        ("an answer is missing", |v| {
            v["answers"].as_array_mut().unwrap().pop();
        }),
        ("answers are out of order", |v| {
            v["answers"].as_array_mut().unwrap().swap(0, 1);
        }),
        ("an answer has another name", |v| {
            v["answers"][0]["name"] = json!("other")
        }),
        ("an answer has no name", |v| {
            v["answers"][0].as_object_mut().unwrap().remove("name");
        }),
        ("a probability above 1", |v| {
            v["answers"][0]["probability"] = json!(1.5)
        }),
        ("a negative probability", |v| {
            v["answers"][0]["probability"] = json!(-0.2)
        }),
        ("a predicate question answered with a score", |v| {
            v["answers"][0] = v["answers"][1].clone();
            v["answers"][0]["name"] = json!("simple");
        }),
        ("a score above the top level", |v| {
            v["answers"][1]["score"] = json!(2.5)
        }),
        ("a confidence above 1", |v| {
            v["answers"][1]["confidence"] = json!(1.2)
        }),
        ("a level label that differs from the question", |v| {
            v["answers"][1]["probabilities"][2]["label"] = json!("extreme");
        }),
        ("a level that does not exist", |v| {
            v["answers"][1]["probabilities"][2]["value"] = json!(5);
        }),
        ("a level twice", |v| {
            v["answers"][1]["probabilities"][2] = v["answers"][1]["probabilities"][1].clone();
        }),
        ("a level missing", |v| {
            v["answers"][1]["probabilities"]
                .as_array_mut()
                .unwrap()
                .pop();
        }),
        ("a predicate answer without its probability", |v| {
            v["answers"][0]
                .as_object_mut()
                .unwrap()
                .remove("probability");
        }),
        ("an answer type the questions never ask", |v| {
            v["answers"][0] = json!({"type": "choice", "name": "simple", "choice": "yes"});
        }),
    ];
    for (situation, mutate) in cases {
        let mut value = good.clone();
        mutate(&mut value);
        let result = parse(&value).and_then(|response| response.validate_against(&request));
        assert!(
            matches!(result, Err(Error::Protocol(_))),
            "{situation}: {result:?}"
        );
    }
    for body in [
        &b"not json"[..],
        b"{}",
        b"{\"model\":\"gpt-6-luna\"}",
        b"[]",
    ] {
        assert!(matches!(
            DecisionResponse::parse(body),
            Err(Error::Protocol(_))
        ));
    }
}

#[test]
fn requests_are_checked_before_sending() {
    let good = request("decisions/router.request.json");
    let mut no_questions = good.clone();
    no_questions.questions.clear();
    let mut duplicate = good.clone();
    duplicate.questions.push(duplicate.questions[0].clone());
    let mut one_level = good.clone();
    one_level.questions[1] = Question::Score {
        name: "effort".into(),
        instructions: "How much reasoning?".into(),
        levels: vec![ScoreLevel {
            label: "only".into(),
            description: None,
        }],
    };
    let image = |url: &str| InputPart::Image {
        image_url: url.into(),
        detail: None,
    };
    let with_parts = |parts: Vec<InputPart>| DecisionRequest {
        input: DecisionInput::Messages(vec![InputMessage {
            role: InputRole::User,
            content: parts,
        }]),
        ..good.clone()
    };
    let public_url = with_parts(vec![image("https://example.com/meter.png")]);
    let other_type = with_parts(vec![image("data:image/svg+xml;base64,PHN2Zy8+")]);
    let png = InputPart::image_base64("image/png", &[0x89, b'P', b'N', b'G'], None).unwrap();
    let too_many = with_parts(vec![png.clone(); ai_clients::decisions::MAX_IMAGES + 1]);
    let too_many_questions = DecisionRequest {
        questions: (0..=ai_clients::decisions::MAX_QUESTIONS)
            .map(|index| Question::Predicate {
                name: format!("q{index}"),
                instructions: "Is it so?".into(),
            })
            .collect(),
        ..good.clone()
    };
    for (situation, bad) in [
        ("no questions", no_questions),
        ("a duplicate name", duplicate),
        ("a score question with one level", one_level),
        ("an image by public URL", public_url),
        ("an image type the API does not take", other_type),
        ("too many images", too_many),
        ("too many questions", too_many_questions),
    ] {
        assert!(
            matches!(bad.validate(), Err(Error::InvalidRequest(_))),
            "{situation}"
        );
    }
    with_parts(vec![png; ai_clients::decisions::MAX_IMAGES])
        .validate()
        .unwrap();
}

#[test]
fn images_are_sent_as_base64_data_urls() {
    let fixture_request = request("decisions/router.request.json");
    let DecisionInput::Messages(messages) = &fixture_request.input else {
        panic!("the router fixture has messages");
    };
    let InputPart::Image { image_url, .. } = &messages[0].content[1] else {
        panic!("part 1 is the image");
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image_url.strip_prefix("data:image/png;base64,").unwrap())
        .unwrap();
    assert_eq!(
        InputPart::image_base64("image/png", &bytes, Some("low")).unwrap(),
        messages[0].content[1]
    );
    assert!(matches!(
        InputPart::image_base64("image/bmp", &bytes, None),
        Err(Error::InvalidRequest(_))
    ));
}

fn client(server: &Server, max_concurrent: usize) -> DecisionsClient {
    DecisionsClient::new(
        endpoint(server.addr, max_concurrent),
        support::test_key(),
        &ai_clients::Tls::none(),
    )
    .expect("a valid endpoint")
}

#[tokio::test]
async fn the_client_sends_the_request_and_checks_the_answers() {
    let ok = vec![Step::Send(http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-request-id", "req_decision"),
        ],
        fixture("decisions/router.response.json").as_bytes(),
    ))];
    let server = Server::start(vec![ok]).await;
    let request = request("decisions/router.request.json");
    let reply = client(&server, 4).decide(&request).await.unwrap();
    assert_eq!(reply.request_id.as_deref(), Some("req_decision"));
    assert_eq!(
        reply.body.answer("simple"),
        Some(&Answer::Predicate {
            name: Some("simple".into()),
            probability: 0.08
        })
    );
    let sent = server.request(0);
    assert!(
        sent.head.starts_with("POST /v1/decisions HTTP/1.1\r\n"),
        "{}",
        sent.head
    );
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {}", support::key_text()))
    );
    assert_eq!(sent.header("x-api-key"), None);
    assert_eq!(sent.json(), fixture_json("decisions/router.request.json"));
}

#[tokio::test]
async fn the_client_refuses_answers_that_do_not_fit_and_invalid_requests() {
    let mut wrong = fixture_json("decisions/router.response.json");
    wrong["answers"].as_array_mut().unwrap().pop();
    let reply = vec![Step::Send(http_response(
        200,
        &[("content-type", "application/json")],
        &serde_json::to_vec(&wrong).unwrap(),
    ))];
    let server = Server::start(vec![reply]).await;
    let client = client(&server, 4);
    let request = request("decisions/router.request.json");
    let error = client.decide(&request).await.unwrap_err();
    // The provider billed the call: its usage comes with the error.
    let Error::InvalidResponse { usage, .. } = &error else {
        panic!("expected an invalid response, got {error:?}");
    };
    assert_eq!(
        usage,
        &ReportedUsage::Attempts(vec![(
            None,
            TokenUsage {
                input: 187,
                ..TokenUsage::default()
            }
        )])
    );
    assert_eq!(error.class(), ErrorClass::Other);
    assert_eq!(server.request_count(), 1);

    let mut invalid = request.clone();
    invalid.questions.clear();
    let error = client.decide(&invalid).await.unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    tokio::task::yield_now().await;
    assert_eq!(
        server.request_count(),
        1,
        "an invalid request reached the server"
    );
}

#[tokio::test]
async fn openai_errors_are_classified() {
    let error_body = |kind: &str, code: &str| {
        serde_json::to_vec(
            &json!({"error": {"message": "m", "type": kind, "param": null, "code": code}}),
        )
        .unwrap()
    };
    let cases = [
        (
            429,
            Some("2"),
            error_body("requests", "rate_limit_exceeded"),
            ErrorClass::RateLimited {
                retry_after: Some(Duration::from_secs(2)),
            },
        ),
        (
            429,
            None,
            error_body("insufficient_quota", "insufficient_quota"),
            ErrorClass::RateLimited { retry_after: None },
        ),
        (
            503,
            None,
            error_body("server_error", "overloaded"),
            ErrorClass::Overloaded,
        ),
        (
            504,
            None,
            b"gateway timeout".to_vec(),
            ErrorClass::Timeout(TimeoutKind::Provider),
        ),
        (
            400,
            None,
            error_body("invalid_request_error", "invalid_value"),
            ErrorClass::Other,
        ),
        (
            500,
            None,
            error_body("server_error", "server_error"),
            ErrorClass::Other,
        ),
    ];
    for (status, retry, body, expected) in cases {
        let mut headers = vec![
            ("content-type", "application/json"),
            ("x-request-id", "req_err"),
        ];
        if let Some(retry) = retry {
            headers.push(("retry-after", retry));
        }
        let server = Server::start(vec![vec![Step::Send(http_response(
            status, &headers, &body,
        ))]])
        .await;
        let error = client(&server, 4)
            .decide(&request("decisions/predicate.request.json"))
            .await
            .unwrap_err();
        assert_eq!(error.class(), expected, "HTTP {status}");
        assert_eq!(error.request_id(), Some("req_err"));
    }
}

#[tokio::test]
async fn decisions_over_the_concurrency_limit_are_not_sent() {
    let server = Server::start(vec![vec![Step::Hang]]).await;
    let client = client(&server, 1);
    let first = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .decide(&request("decisions/predicate.request.json"))
                .await
        })
    };
    server.wait_for_requests(1).await;
    let error = client
        .decide(&request("decisions/predicate.request.json"))
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::ConcurrencyLimited { .. }),
        "{error:?}"
    );
    tokio::task::yield_now().await;
    assert_eq!(server.request_count(), 1);
    first.abort();
}

#[tokio::test]
async fn raw_decisions_go_out_byte_for_byte_and_are_checked_like_decide() {
    let response = fixture("decisions/router.response.json");
    let server = Server::start(vec![vec![Step::Send(http_response(
        200,
        &[
            ("content-type", "application/json"),
            ("x-request-id", "req_raw"),
        ],
        response.as_bytes(),
    ))]])
    .await;
    let client = client(&server, 4);
    // The fixture as written: its indentation and key order go out unchanged.
    let body = fixture("decisions/router.request.json");
    let reply = client
        .decide_raw(bytes::Bytes::from(body.clone()))
        .await
        .unwrap();
    assert_eq!(server.request(0).body, body.as_bytes());
    assert_eq!(reply.raw, response.as_bytes());
    assert_eq!(
        reply.body,
        DecisionResponse::parse(response.as_bytes()).unwrap()
    );
    assert_eq!(reply.request_id.as_deref(), Some("req_raw"));

    // Invalid requests are refused before anything is sent.
    let mut no_questions = fixture_json("decisions/router.request.json");
    no_questions["questions"] = json!([]);
    for invalid in [
        serde_json::to_vec(&no_questions).unwrap(),
        b"not json".to_vec(),
    ] {
        let error = client
            .decide_raw(bytes::Bytes::from(invalid))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }
    assert_eq!(server.request_count(), 1);
}
