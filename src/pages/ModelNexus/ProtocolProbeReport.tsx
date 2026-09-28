import { useState } from 'react';
import { ChevronDown, ChevronRight } from 'lucide-react';
import type { TKey } from '../../i18n';
import { useI18n } from '../../hooks/useI18n';
import { API_PROTOCOL_OPTIONS } from './protocolLabels';

/** What one probe of the four dialects found. Mirrors `protocol_probe::DialectReport`. */
export type DialectReport = {
  protocol: string;
  available: boolean;
  /** `available` | `unsupported` | `auth` | `unknown` — not a boolean, because a
   *  bad key and a missing endpoint look identical from the outside and send the
   *  user to fix completely different things. */
  outcome: 'available' | 'unsupported' | 'auth' | 'unknown';
  detail?: string;
  latencyMs: number;
};

/** Reuse the dropdown's own labels so the report reads the same as the picker. */
const PROTOCOL_LABEL_KEYS: Record<string, TKey> = Object.fromEntries(
  API_PROTOCOL_OPTIONS.filter((o) => o.value).map((o) => [o.value, o.label])
);

const OUTCOME_KEYS: Record<DialectReport['outcome'], TKey> = {
  available: 'model.probeOutcome.available',
  unsupported: 'model.probeOutcome.unsupported',
  auth: 'model.probeOutcome.auth',
  unknown: 'model.probeOutcome.unknown',
};

/**
 * The result of a four-dialect protocol probe, collapsible.
 *
 * This block used to be an unconditional list. That is a layout trap in a
 * scrolling form: the report is up to four rows tall, each carrying a
 * protocol name, an outcome and a truncated provider error, so printing it
 * decided how much of the model config remained visible — and because the
 * modal grows downward, the Save/Cancel footer fell off screen with no way to
 * undo it. The user was left staring at their own results with the only
 * controls that let them proceed out of reach.
 *
 * So the verdict goes on one line and the detail folds away. Collapsed, it
 * still says the thing that matters — how many of the four dialects answered —
 * because a report you have to expand to learn whether your provider works is
 * one click further than it needs to be.
 *
 * Expanded state is local and resets when a new probe replaces the result, so
 * a stale expansion cannot carry into the next provider.
 */
export const ProtocolProbeReport: React.FC<{
  reports: DialectReport[];
  error?: string;
  /** Start expanded — used for a re-render that already has the user's focus. */
  defaultExpanded?: boolean;
}> = ({ reports, error, defaultExpanded = false }) => {
  const { t } = useI18n();
  const [expanded, setExpanded] = useState(defaultExpanded);

  if (error) {
    return (
      <p className="mt-1.5 text-[10px] text-amber-500">
        {t('model.probeFailed')}: {error}
      </p>
    );
  }

  if (reports.length === 0) return null;

  const available = reports.filter((entry) => entry.available).length;

  return (
    <div className="mt-1.5">
      <button
        type="button"
        onClick={() => setExpanded((value) => !value)}
        aria-expanded={expanded}
        className="inline-flex items-center gap-1 text-[10px] text-cyber-text-secondary transition-colors hover:text-cyber-text"
      >
        {/* A chevron, not a cursor change: the app is a desktop shell and
            AGENTS.md rules out pointer cursors on controls. */}
        {expanded ? (
          <ChevronDown className="h-3 w-3 shrink-0" aria-hidden />
        ) : (
          <ChevronRight className="h-3 w-3 shrink-0" aria-hidden />
        )}
        <span className="tabular-nums">
          {available}/{reports.length}
        </span>
        <span className="text-cyber-text-tertiary">
          {expanded ? t('model.probeCollapse') : t('model.probeExpand')}
        </span>
      </button>

      {expanded && (
        <ul className="mt-1 space-y-0.5">
          {reports.map((entry) => (
            <li key={entry.protocol} className="flex items-start gap-1.5 text-[10px]">
              <span
                className={
                  entry.available
                    ? 'text-emerald-500'
                    : entry.outcome === 'auth'
                      ? 'text-amber-500'
                      : 'text-red-500'
                }
              >
                {entry.available ? '●' : '○'}
              </span>
              <span className="text-cyber-text-secondary">
                {t(PROTOCOL_LABEL_KEYS[entry.protocol] ?? 'model.apiProtocolChat')}
              </span>
              <span className="text-cyber-text-tertiary">
                {t(OUTCOME_KEYS[entry.outcome])}
                {entry.latencyMs > 0 && ` · ${entry.latencyMs}ms`}
              </span>
              {entry.detail && (
                <span className="truncate opacity-70" title={entry.detail}>
                  {entry.detail}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
};
