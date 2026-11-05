import { EllipsisIcon } from "lucide-react";
import { Link } from "react-router";

import { Button } from "./ui/button.tsx";
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from "./ui/dropdown-menu.tsx";

/** One entry of a row's actions: a link to a page, or an action the page runs. */
export type RowMenuItem =
  | { kind: "link"; label: string; to: string }
  | { kind: "action"; label: string; onSelect(): void; destructive?: boolean };

/**
 * The "…" button at the end of a list row and the menu it opens (docs/design/tenant-admin/, 「操作」).
 * `label` names the row ("line-1-meter actions"), so a screen reader tells the buttons apart.
 */
export function RowMenu({ label, items }: { label: string; items: readonly RowMenuItem[] }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button variant="ghost" size="icon" aria-label={label}>
          <EllipsisIcon aria-hidden="true" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        {items.map((item) =>
          item.kind === "link" ? (
            <DropdownMenuItem key={item.label} asChild>
              <Link to={item.to}>{item.label}</Link>
            </DropdownMenuItem>
          ) : (
            <DropdownMenuItem
              key={item.label}
              variant={item.destructive ? "destructive" : "default"}
              onSelect={item.onSelect}
            >
              {item.label}
            </DropdownMenuItem>
          ),
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
