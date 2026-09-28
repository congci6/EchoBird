import { describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { translate } from '../../i18n';
import { I18nContext } from '../../hooks/i18nContext';
import { ProtocolProbeReport } from './ProtocolProbeReport';

/**
 * The protocol-probe report can be four rows tall, each carrying a protocol
 * name, an outcome and a truncated provider error. In a form that is already
 * scrolling, that block decides how much of the model config the user can see
 * at once — so it has to be collapsible, and its collapsed state has to be
 * legible without expanding it.
 *
 * These tests pin the two states and the control between them, because the
 * bug this replaces was a report that appeared with no way back: the form grew
 * downward, the footer fell off screen, and the user could not undo the
 * expansion to get the Save button back.
 */
describe('ProtocolProbeReport', () => {
  const available = {
    protocol: 'openaiChat' as const,
    available: true,
    outcome: 'available' as const,
    latencyMs: 2930,
    detail: '',
  };
  const unsupported = {
    protocol: 'openaiResponses' as const,
    available: false,
    outcome: 'unsupported' as const,
    latencyMs: 210,
    detail: 'not implemented',
  };

  // Render with real English strings: the default context echoes keys back,
  // which would make every assertion below pass for the wrong reason.
  const render = (props: Parameters<typeof ProtocolProbeReport>[0]) =>
    renderToStaticMarkup(
      <I18nContext.Provider
        value={{ locale: 'en', setLocale: () => {}, t: (k) => translate(k, 'en') }}
      >
        <ProtocolProbeReport {...props} />
      </I18nContext.Provider>
    );

  it('collapses to a single summary line by default', () => {
    const html = render({ reports: [available, unsupported] });

    // The summary carries the verdict, so a collapsed report is still useful.
    expect(html).toContain('1/2');
    // Per-protocol rows are not rendered while collapsed.
    expect(html).not.toContain('not implemented');
  });

  it('renders every protocol row once expanded', () => {
    const html = render({ reports: [available, unsupported], defaultExpanded: true });

    expect(html).toContain('not implemented');
    expect(html).toContain('2930ms');
    expect(html).toContain('210ms');
  });

  it('shows a failed probe as an error rather than a result list', () => {
    const html = render({ reports: [], error: 'Invalid token' });

    expect(html).toContain('Invalid token');
    expect(html).not.toContain('1/0');
  });

  it('does not render at all when there is nothing to report', () => {
    const html = render({ reports: [] });

    expect(html).toBe('');
  });
});
