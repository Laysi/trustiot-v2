import { MessageSquarePlusIcon } from "lucide-react";
import { Link } from "react-router";

import { newConversationPath } from "../../app/paths.ts";
import { Button, type ButtonProps } from "../../components/ui/button.tsx";

/**
 * The entries that carry a user into a conversation with a question to finish and send
 * (「在對話中編輯」 on a dashboard, 「在對話中新增」 on the list; docs/design/chat-dashboards/design.md):
 * a link to the conversation home with `question` in the composer. Who may use it (editors and
 * administrators) is the page's decision.
 */
export function StartInConversation({
  question,
  children,
  variant = "default",
}: {
  question: string;
  children: string;
  variant?: ButtonProps["variant"];
}) {
  return (
    <Button asChild variant={variant}>
      <Link to={newConversationPath(question)}>
        <MessageSquarePlusIcon />
        {children}
      </Link>
    </Button>
  );
}
