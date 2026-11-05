import { LockIcon } from "lucide-react";
import { Link } from "react-router";

import { Button } from "../components/ui/button.tsx";
import { StatusPage } from "./StatusPage.tsx";

/**
 * What a role that may not write sees at the address of a write form (a viewer who opens
 * `/devices/new` from a link): why, and the way back, instead of a form the server would refuse.
 */
export function WriteDenied({
  title,
  body,
  backTo,
  back,
}: {
  title: string;
  body: string;
  backTo: string;
  back: string;
}) {
  return (
    <StatusPage
      title={title}
      icon={<LockIcon aria-hidden="true" />}
      actions={
        <Button variant="outline" asChild>
          <Link to={backTo}>{back}</Link>
        </Button>
      }
    >
      <p>{body}</p>
    </StatusPage>
  );
}
