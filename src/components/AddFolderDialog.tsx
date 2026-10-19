import { useEffect, useRef, useState } from "react";

import type { FileKind } from "../api/types";
import { DEFAULT_KINDS } from "../api/types";
import { t } from "../i18n";
import { KindPicker } from "./KindPicker";

interface Props {
  paths: string[];
  onConfirm: (kinds: FileKind[]) => void;
  onCancel: () => void;
}

/** Shown after folders are picked: which file kinds to index (all but .log by default). */
export function AddFolderDialog({ paths, onConfirm, onCancel }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [kinds, setKinds] = useState<FileKind[]>(DEFAULT_KINDS);

  useEffect(() => {
    const element = dialog.current;
    if (element && !element.open) element.showModal();
    return () => element?.close();
  }, []);

  return (
    <dialog
      ref={dialog}
      className="dialog"
      aria-labelledby="add-folder-title"
      onCancel={(event) => {
        event.preventDefault();
        onCancel();
      }}
    >
      <form
        method="dialog"
        onSubmit={(event) => {
          event.preventDefault();
          if (kinds.length > 0) onConfirm(kinds);
        }}
      >
        <h2 id="add-folder-title">{paths.length > 1 ? t.kinds.addTitleMany(paths.length) : t.kinds.addTitle}</h2>
        <ul className="dialog-paths">
          {paths.map((path) => (
            <li key={path} className="dialog-path" title={path}>
              <bdi>{path}</bdi>
            </li>
          ))}
        </ul>
        <KindPicker value={kinds} onChange={setKinds} />
        <p className="dialog-help">{t.kinds.help}</p>
        <div className="dialog-actions">
          <button type="button" className="btn" onClick={onCancel}>
            {t.kinds.cancel}
          </button>
          <button type="submit" className="btn primary" disabled={kinds.length === 0}>
            {paths.length > 1 ? t.kinds.addMany(paths.length) : t.kinds.add}
          </button>
        </div>
      </form>
    </dialog>
  );
}
