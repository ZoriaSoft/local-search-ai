import type { FileKind } from "../api/types";
import { t } from "../i18n";

const SHORT: Record<FileKind, string> = { pdf: "PDF", image: "IMG", word: "DOC", excel: "XLS", text: "TXT", log: "LOG" };

/** A small labelled document tile; colour tells the kind apart at a glance. */
export function KindTile({ kind }: { kind: FileKind }) {
  return (
    <span className={`kind-tile k-${kind}`} title={t.kind[kind]} aria-label={t.kind[kind]}>
      {SHORT[kind]}
    </span>
  );
}
