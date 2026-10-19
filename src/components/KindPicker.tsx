import type { FileKind } from "../api/types";
import { DEFAULT_KINDS, FILE_KINDS } from "../api/types";
import { t } from "../i18n";

interface Props {
  value: FileKind[];
  onChange: (kinds: FileKind[]) => void;
  legend?: string;
}

/** One checkbox per file kind; the parent refuses to save an empty choice. */
export function KindPicker({ value, onChange, legend = t.kinds.title }: Props) {
  const toggle = (kind: FileKind) =>
    onChange(value.includes(kind) ? value.filter((k) => k !== kind) : FILE_KINDS.filter((k) => k === kind || value.includes(k)));
  return (
    <fieldset className="kind-picker">
      <legend>{legend}</legend>
      {FILE_KINDS.map((kind) => (
        <label key={kind}>
          <input type="checkbox" checked={value.includes(kind)} onChange={() => toggle(kind)} />
          <span>{t.kind[kind]}</span>
          <span className="kind-files">{t.kindFiles[kind]}</span>
        </label>
      ))}
      {value.length === 0 && (
        <p className="kind-error" role="alert">
          {t.errors.kinds_empty}
        </p>
      )}
    </fieldset>
  );
}

/** "Tüm türler", "Tüm türler (log hariç)" or the selected kinds' names. */
export function kindSummary(kinds: FileKind[]): string {
  if (FILE_KINDS.every((k) => kinds.includes(k))) return t.kinds.all;
  if (kinds.length === DEFAULT_KINDS.length && DEFAULT_KINDS.every((k) => kinds.includes(k))) return t.kinds.allButLog;
  return t.kinds.summary(kinds.map((k) => t.kind[k]));
}
