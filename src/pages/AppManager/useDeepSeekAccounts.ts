import { useCallback, useEffect, useRef, useState } from 'react';
import * as api from '../../api/tauri';
import { accountError } from '../../utils/accountError';
import { useI18n } from '../../hooks/useI18n';
import { useConfirm } from '../../components/ConfirmDialog';

const LOGIN_TIMEOUT_SECONDS = 60;

export function useDeepSeekAccounts(
  enabled: boolean,
  hasModel: boolean,
  clearModel: () => void,
  showError: (error: string) => void
) {
  const { t, locale } = useI18n();
  const confirm = useConfirm();
  const [accounts, setAccounts] = useState<api.DeepSeekAccount[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [remainingSeconds, setRemainingSeconds] = useState(0);
  const [refreshing, setRefreshing] = useState<Set<string>>(new Set());
  const generation = useRef(0);
  const adding = useRef(false);
  const selectionRevision = useRef(0);
  const pending = useRef<api.DeepSeekLogin | null>(null);
  const refreshIds = useRef(new Set<string>());
  const hasModelRef = useRef(hasModel);
  useEffect(() => {
    hasModelRef.current = hasModel;
  }, [hasModel]);

  const reload = useCallback(async () => {
    const current = generation.current;
    const result = await api.listDeepSeekAccounts();
    if (current === generation.current) setAccounts(result);
  }, []);

  useEffect(() => {
    const current = ++generation.current;
    const timer = setTimeout(() => {
      setBusy(false);
      setRefreshing(new Set());
      if (!enabled) return;
      void api
        .listDeepSeekAccounts()
        .then((result) => {
          if (current !== generation.current) return;
          setAccounts(result);
          if (!hasModelRef.current)
            setSelected((prev) =>
              result.some((a) => a.id === prev) ? prev : (result.find((a) => a.active)?.id ?? null)
            );
        })
        .catch((error) => {
          if (current === generation.current) showError(accountError(error, t));
        });
    }, 0);
    return () => {
      clearTimeout(timer);
      generation.current += 1;
      adding.current = false;
      refreshIds.current = new Set();
      const login = pending.current;
      pending.current = null;
      if (login) void api.cancelDeepSeekLogin(login.loginId).catch(() => {});
    };
  }, [enabled, showError, t]);

  const select = (id: string | null) => {
    selectionRevision.current += 1;
    setSelected(id);
    if (id) clearModel();
  };

  const refresh = async (account: api.DeepSeekAccount, quiet = false) => {
    const ids = refreshIds.current;
    if (ids.has(account.id)) return;
    const current = generation.current;
    ids.add(account.id);
    setRefreshing(new Set(ids));
    try {
      const updated = await api.refreshDeepSeekAccountQuota(account.id, locale);
      if (current === generation.current)
        setAccounts((prev) => prev.map((a) => (a.id === updated.id ? updated : a)));
    } catch (error) {
      if (!quiet && current === generation.current) showError(accountError(error, t));
    } finally {
      ids.delete(account.id);
      if (current === generation.current) setRefreshing(new Set(ids));
    }
  };

  const add = async () => {
    if (!enabled || adding.current) return;
    const current = generation.current;
    adding.current = true;
    const selectionAtStart = selectionRevision.current;
    setBusy(true);
    setRemainingSeconds(LOGIN_TIMEOUT_SECONDS);
    let login: api.DeepSeekLogin | null = null;
    let ticker: ReturnType<typeof setInterval> | undefined;
    try {
      login = await api.startDeepSeekLogin(locale);
      if (current !== generation.current) return;
      pending.current = login;
      const expires = login.expiresAt;
      setRemainingSeconds(Math.max(0, Math.ceil(expires - Date.now() / 1000)));
      ticker = setInterval(() => {
        if (current === generation.current)
          setRemainingSeconds(Math.max(0, Math.ceil(expires - Date.now() / 1000)));
      }, 250);
      await api.openExternal(login.verificationUri);
      while (current === generation.current && Date.now() / 1000 < expires) {
        const account = await api.pollDeepSeekLogin(login.loginId);
        if (current !== generation.current) return;
        if (account) {
          pending.current = null;
          await reload();
          if (current !== generation.current) return;
          // Do not replace a model explicitly selected while the browser was open.
          if (selectionRevision.current === selectionAtStart) select(account.id);
          void refresh(account, true);
          return;
        }
        await new Promise((resolve) => setTimeout(resolve, 1500));
      }
      if (current === generation.current) throw new Error('accountError.expired');
    } catch (error) {
      if (current === generation.current) showError(accountError(error, t));
    } finally {
      clearInterval(ticker);
      if (login) void api.cancelDeepSeekLogin(login.loginId).catch(() => {});
      if (current === generation.current) {
        pending.current = null;
        adding.current = false;
        setBusy(false);
      }
    }
  };

  const remove = async (account: api.DeepSeekAccount) => {
    const current = generation.current;
    if (
      !(await confirm({
        title: t('agent.deleteAccountTitle'),
        confirmText: t('btn.delete'),
        type: 'danger',
        message: t('agent.deleteAccountConfirm').replace('{email}', account.name),
      }))
    )
      return;
    try {
      await api.deleteDeepSeekAccount(account.id);
      if (current !== generation.current) return;
      setAccounts((prev) => prev.filter((a) => a.id !== account.id));
      setSelected((prev) => (prev === account.id ? null : prev));
    } catch (error) {
      if (current === generation.current) showError(accountError(error, t));
    }
  };

  return {
    accounts,
    selectedId: enabled && !hasModel ? selected : null,
    select,
    busy,
    remainingSeconds,
    refreshing,
    add,
    refresh,
    remove,
    reload,
  };
}
