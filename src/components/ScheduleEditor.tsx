import { useState } from "react";

import type { FolderSchedule } from "../api/types";
import { t } from "../i18n";

const ALL_DAYS = [0, 1, 2, 3, 4, 5, 6];
/** A new timed folder starts at a quiet hour: every day, 02:00 to 06:00. */
export const DEFAULT_SCHEDULE: FolderSchedule = { days: ALL_DAYS, start: 2 * 60, end: 6 * 60 };

const pad = (n: number) => String(n).padStart(2, "0");
export const clock = (minutes: number) => `${pad(Math.floor(minutes / 60))}:${pad(minutes % 60)}`;
const minutesOf = (value: string) => {
  const [h = NaN, m = NaN] = value.split(":").map(Number);
  return Number.isFinite(h) && Number.isFinite(m) ? h * 60 + m : 0;
};

/** "Her gün 02:00–06:00", "Pzt, Çar 22:00–06:00 (ertesi gün)" or "Anında". */
export function scheduleSummary(schedule: FolderSchedule | null | undefined): string {
  if (!schedule) return t.schedule.summaryLive;
  const days =
    schedule.days.length === 7
      ? t.schedule.everyDay
      : [...schedule.days].sort().map((d) => t.schedule.dayNames[d]).join(", ");
  return t.schedule.summaryTimed(days, clock(schedule.start), clock(schedule.end), schedule.end <= schedule.start);
}

interface Props {
  value: FolderSchedule | null;
  onSave: (schedule: FolderSchedule | null) => void;
  onCancel: () => void;
}

/** Live watching or a daily/weekly time window for one folder. */
export function ScheduleEditor({ value, onSave, onCancel }: Props) {
  const [timed, setTimed] = useState(value !== null);
  const [draft, setDraft] = useState<FolderSchedule>(value ?? DEFAULT_SCHEDULE);
  const everyDay = draft.days.length === 7;
  const toggleDay = (day: number) =>
    setDraft({ ...draft, days: draft.days.includes(day) ? draft.days.filter((d) => d !== day) : [...draft.days, day].sort() });
  const invalid = timed && (draft.days.length === 0 || draft.start === draft.end);
  const overnight = draft.end < draft.start;

  return (
    <form
      className="kind-editor schedule-editor"
      onSubmit={(event) => {
        event.preventDefault();
        if (!invalid) onSave(timed ? draft : null);
      }}
    >
      <fieldset className="schedule-mode">
        <legend>{t.schedule.title}</legend>
        <label>
          <input type="radio" name="schedule-mode" checked={!timed} onChange={() => setTimed(false)} />
          <span>
            <strong>{t.schedule.live}</strong>
            <small>{t.schedule.liveHelp}</small>
          </span>
        </label>
        <label>
          <input type="radio" name="schedule-mode" checked={timed} onChange={() => setTimed(true)} />
          <span>
            <strong>{t.schedule.timed}</strong>
            <small>{t.schedule.timedHelp}</small>
          </span>
        </label>
      </fieldset>

      {timed && (
        <div className="schedule-window">
          <div className="schedule-days" role="group" aria-label={t.schedule.days}>
            <button
              type="button"
              className={`chip${everyDay ? " on" : ""}`}
              aria-pressed={everyDay}
              onClick={() => setDraft({ ...draft, days: everyDay ? [] : ALL_DAYS })}
            >
              {t.schedule.everyDay}
            </button>
            {ALL_DAYS.map((day) => (
              <button
                key={day}
                type="button"
                className={`chip${draft.days.includes(day) ? " on" : ""}`}
                aria-pressed={draft.days.includes(day)}
                title={t.schedule.dayLong[day]}
                onClick={() => toggleDay(day)}
              >
                {t.schedule.dayNames[day]}
              </button>
            ))}
          </div>
          <div className="schedule-times">
            <label>
              <span>{t.schedule.from}</span>
              <input type="time" required value={clock(draft.start)} onChange={(e) => setDraft({ ...draft, start: minutesOf(e.target.value) })} />
            </label>
            <label>
              <span>{t.schedule.to}</span>
              <input type="time" required value={clock(draft.end)} onChange={(e) => setDraft({ ...draft, end: minutesOf(e.target.value) })} />
            </label>
            {overnight && <span className="muted">{t.schedule.overnight}</span>}
          </div>
          {invalid && (
            <p className="kind-error" role="alert">
              {t.errors.schedule_invalid}
            </p>
          )}
          <p className="muted schedule-note">{t.schedule.note}</p>
        </div>
      )}

      <div className="form-actions">
        <button type="button" className="btn small" onClick={onCancel}>
          {t.kinds.cancel}
        </button>
        <button type="submit" className="btn small primary" disabled={invalid}>
          {t.kinds.save}
        </button>
      </div>
    </form>
  );
}
