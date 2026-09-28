import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { DeepSeekAccountSection } from './DeepSeekAccountSection';
import { AppManagerContext, type AppManagerContextType } from './context';
import type { DeepSeekAccount } from '../../api/tauri';

function render(balances: DeepSeekAccount['balances'], busy = false) {
  const context = {
    deepSeekAccounts: {
      accounts: [{ id: 'one', name: 'DeepSeek user', balances, active: true }],
      selectedId: 'one',
      refreshing: new Set(['one']),
      busy,
      remainingSeconds: 120,
    },
  } as AppManagerContextType;
  return renderToStaticMarkup(
    <AppManagerContext.Provider value={context}>
      <DeepSeekAccountSection showDivider={false} />
    </AppManagerContext.Provider>
  );
}

describe('DeepSeek accounts', () => {
  it('shows balances in their own currencies and uses existing controls', () => {
    const markup = render([
      { currency: 'CNY', amount: 12.34 },
      { currency: 'USD', amount: 5.67 },
    ]);
    expect(markup).toContain('12.34');
    expect(markup).toContain('5.67');
    expect(markup).toContain('aria-checked="true"');
    expect(markup).toContain('deepseek-account-pill');
    expect(markup).toContain('/icons/tools/dsh.png');
    expect(markup).toContain('disabled=""');
    expect(markup).not.toContain('title=');
    expect(markup).not.toContain('width:');
  });
  it('distinguishes unknown balance from zero and reuses the waiting label', () => {
    expect(render(null)).toContain('—');
    expect(render([{ currency: 'CNY', amount: 0 }])).toContain('0.00');
    expect(render(null, true)).not.toContain('agent.addCurrentAccount');
  });
});
