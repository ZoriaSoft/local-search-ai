import type { Range } from "../api/types";

/** Renders text with <mark> around UTF-16 ranges; never uses innerHTML. */
export function Highlighted({ text, ranges }: { text: string; ranges: Range[] }) {
  const parts: React.ReactNode[] = [];
  let cursor = 0;
  const sorted = [...ranges].sort((a, b) => a[0] - b[0]);
  sorted.forEach(([start, end], i) => {
    if (start < cursor || end > text.length || end <= start) return;
    if (start > cursor) parts.push(text.slice(cursor, start));
    parts.push(<mark key={i}>{text.slice(start, end)}</mark>);
    cursor = end;
  });
  if (cursor < text.length) parts.push(text.slice(cursor));
  return <>{parts}</>;
}
