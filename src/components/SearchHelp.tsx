import { useEffect, useRef } from "react";

import { t } from "../i18n";

interface Props {
  /** Puts an example into the search box and closes the guide. */
  onTry: (query: string) => void;
  onClose: () => void;
}

/** "Nasıl aranır?": every search method with examples one click away. */
export function SearchHelp({ onTry, onClose }: Props) {
  const dialog = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const element = dialog.current;
    if (element && !element.open) element.showModal();
    return () => element?.close();
  }, []);

  return (
    <dialog
      ref={dialog}
      className="dialog help-dialog"
      aria-labelledby="search-help-title"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      onClick={(event) => {
        if (event.target === dialog.current) onClose();
      }}
    >
      <div className="help-body">
        <h2 id="search-help-title">{t.help.title}</h2>
        <p className="help-lead">{t.help.lead}</p>
        {t.help.sections.map((section) => (
          <section key={section.title} className="help-section">
            <h3>{section.title}</h3>
            <ul>
              {section.items.map((item) => (
                <li key={item.example}>
                  <button type="button" className="help-example" onClick={() => onTry(item.example)} title={t.help.tryIt}>
                    {item.example}
                  </button>
                  <span>{item.text}</span>
                </li>
              ))}
            </ul>
          </section>
        ))}
        <p className="help-note">{t.help.note}</p>
        <div className="dialog-actions">
          <button type="button" className="btn primary" onClick={onClose}>
            {t.help.close}
          </button>
        </div>
      </div>
    </dialog>
  );
}
