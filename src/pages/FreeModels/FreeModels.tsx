import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentPropsWithRef,
  type ReactNode,
} from 'react';
import { createPortal } from 'react-dom';
import {
  DndContext,
  DragOverlay,
  PointerSensor,
  KeyboardSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from '@dnd-kit/core';
import {
  SortableContext,
  useSortable,
  rectSortingStrategy,
  sortableKeyboardCoordinates,
  arrayMove,
} from '@dnd-kit/sortable';
import { CSS } from '@dnd-kit/utilities';
import { open as shellOpen } from '@tauri-apps/plugin-shell';
import {
  Box,
  Check,
  CircleHelp,
  ExternalLink,
  KeyRound,
  Plus,
  RefreshCw,
  Route,
  SquarePen,
  Trash2,
  X,
} from 'lucide-react';
import * as api from '../../api/tauri';
import type { FreeModelDirectory, FreeModelEntry } from '../../api/freeModels';
import { getModelIcon } from '../../components';
import { ModelListCard } from '../../components/ModelListCard';
import { useConfirm } from '../../components/ConfirmDialog';
import { useToast } from '../../components/Toast';
import { useI18n } from '../../hooks/useI18n';
import { useNavigationStore } from '../../stores/navigationStore';
import { useModelNexus } from '../ModelNexus/context';
import './FreeModels.css';

const NODE_COLUMN_GAP = 28;
const NODE_ROW_GAP = 56;
const HUB_TO_NODE_GAP = 72;
const HUB_ARROW_GAP = 4;
const HUB_ARROW_HEIGHT = 12;
const HUB_ARROW_LINE_GAP = 2;
const SCAN_STEP_MS = 230;
const SCAN_COMPLETE_MS = 320;
const ROUTER_ACTIVITY_POLL_MS = 750;
const ROUTER_ACTIVITY_GRACE_MS = 2200;

interface FreeModelsContextValue {
  catalog: FreeModelDirectory;
  models: FreeModelEntry[];
  customModels: RouteModelNode[];
  selectedIds: Set<string>;
  refreshing: boolean;
  scanProvider: string;
  scanProgress: number;
  routerBaseUrl: string;
  routerEnabled: boolean;
  routerTogglePending: boolean;
  setRouterEnabled: (enabled: boolean) => Promise<void>;
  refresh: () => Promise<void>;
  addSelectedModel: (model: RouteModelInput) => Promise<void>;
  updateSelectedModel: (model: RouteModelInput) => void;
  removeSelectedModel: (id: string) => Promise<void>;
  reorderSelectedModel: (activeId: string, overId: string) => Promise<void>;
}

interface RouteModelInput {
  internalId: string;
  name: string;
  baseUrl: string;
  modelId: string;
}

interface RouteModelNode {
  id: string;
  internalId: string;
  provider: string;
  modelId: string;
  baseUrl: string;
}

interface FreeModelProviderGroup {
  id: string;
  name: string;
  baseUrl: string;
  docsUrl: string;
  models: FreeModelEntry[];
}

interface RoutePath {
  id: string;
  d: string;
}

interface RouteArrow {
  x: number;
  y: number;
}

const FreeModelsContext = createContext<FreeModelsContextValue | null>(null);
const emptyCatalog: FreeModelDirectory = {
  version: 1,
  updatedAt: '',
  models: [],
};
const providerPriority = ['nvidia-nim', 'openrouter'];

function providerRank(model: FreeModelEntry): number {
  const rank = providerPriority.indexOf(model.providerId);
  return rank === -1 ? providerPriority.length : rank;
}

function groupModelsForScan(models: FreeModelEntry[]): FreeModelEntry[][] {
  const groups = new Map<string, FreeModelEntry[]>();
  models.forEach((model) => {
    const id = `${model.providerId}:${model.baseUrl}`;
    const group = groups.get(id);
    if (group) group.push(model);
    else groups.set(id, [model]);
  });
  return [...groups.values()].sort((left, right) => providerRank(left[0]) - providerRank(right[0]));
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}

export function useFreeModels() {
  const value = useContext(FreeModelsContext);
  if (!value) throw new Error('useFreeModels must be used within FreeModelsProvider');
  return value;
}

function shortModelName(modelId: string): string {
  const tail = modelId.split('/').pop() || modelId;
  return tail.length > 27 ? `${tail.slice(0, 24)}…` : tail;
}

export function FreeModelsProvider({ children }: { children: ReactNode }) {
  const { t } = useI18n();
  const { showToast } = useToast();
  const activePage = useNavigationStore((state) => state.activePage);
  const [catalog, setCatalog] = useState<FreeModelDirectory>(emptyCatalog);
  const [customModels, setCustomModels] = useState<RouteModelNode[]>([]);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());
  const [refreshing, setRefreshing] = useState(false);
  const [scanProvider, setScanProvider] = useState('');
  const [scanProgress, setScanProgress] = useState(0);
  const [routerBaseUrl, setRouterBaseUrl] = useState('127.0.0.1:53683/v1');
  const [routerEnabled, setRouterEnabledState] = useState(true);
  const [routerTogglePending, setRouterTogglePending] = useState(false);
  const [routerLoaded, setRouterLoaded] = useState(false);
  const refreshInFlightRef = useRef(false);
  const routerMutationRef = useRef<Promise<void>>(Promise.resolve());
  const selectedIdsRef = useRef<Set<string>>(new Set());
  const routerLoadedRef = useRef(false);
  const models = catalog.models;

  const loadRouter = useCallback(async () => {
    routerLoadedRef.current = false;
    const [loadedRouter, configuredModels] = await Promise.all([
      api.getSmartRouterConfig(),
      api.getSmartRouterCandidates(),
    ]);
    const modelsById = new Map(configuredModels.map((model) => [model.internalId, model]));
    const routeModels = loadedRouter.candidateIds.flatMap((internalId) => {
      const model = modelsById.get(internalId);
      if (!model) return [];
      return [
        {
          id: internalId,
          internalId,
          provider: model.name,
          modelId: model.modelId ?? '',
          baseUrl: model.baseUrl,
        },
      ];
    });
    const router =
      loadedRouter.enabled && routeModels.length === 0
        ? await api.setSmartRouterEnabled(false)
        : loadedRouter;
    setCustomModels(routeModels);
    const next = new Set(router.candidateIds);
    selectedIdsRef.current = next;
    setSelectedIds((current) => {
      if (
        current.size === next.size &&
        [...current].every((id, index) => id === router.candidateIds[index])
      ) {
        return current;
      }
      return next;
    });
    setRouterBaseUrl(router.baseUrl.replace(/^https?:\/\//, ''));
    setRouterEnabledState(router.enabled);
    setRouterLoaded(true);
    routerLoadedRef.current = true;
  }, []);

  useEffect(() => {
    if (routerLoadedRef.current && activePage !== 'freeModels') return;
    // Keep reads and their state updates in the mutation queue, so a late
    // page-entry response cannot overwrite a newer addition or priority order.
    const load = routerMutationRef.current.catch(() => undefined).then(loadRouter);
    routerMutationRef.current = load;
    void load.catch((error) => console.error('Load smart router config failed:', error));
  }, [activePage, loadRouter]);

  const setRouterEnabled = useCallback(
    async (enabled: boolean) => {
      setRouterTogglePending(true);
      const change = routerMutationRef.current
        .catch(() => undefined)
        .then(async () => {
          const router = await api.setSmartRouterEnabled(enabled);
          setRouterEnabledState(router.enabled);
          setRouterBaseUrl(router.baseUrl.replace(/^https?:\/\//, ''));
        });
      routerMutationRef.current = change;
      try {
        await change;
      } catch (error) {
        console.error('Toggle smart router failed:', error);
        showToast('error', t('error.requestFailed'));
      } finally {
        setRouterTogglePending(false);
      }
    },
    [showToast, t]
  );

  const refresh = useCallback(async () => {
    if (refreshInFlightRef.current) return;
    refreshInFlightRef.current = true;
    setCatalog(emptyCatalog);
    setScanProvider('');
    setScanProgress(0);
    setRefreshing(true);
    try {
      const remote = await api.getFreeModelDirectory();
      if (!remote) {
        showToast('warning', t('freeModels.fetchFailed'));
        return;
      }

      const groups = groupModelsForScan(remote.models);
      const revealedModels: FreeModelEntry[] = [];
      for (const [index, group] of groups.entries()) {
        setScanProvider(group[0]?.provider ?? '');
        await wait(SCAN_STEP_MS);
        revealedModels.push(...group);
        setCatalog({ ...remote, models: [...revealedModels] });
        setScanProgress((index + 1) / groups.length);
      }
      await wait(SCAN_COMPLETE_MS);
    } catch (error) {
      console.error('Fetch free model directory failed:', error);
      showToast('error', t('freeModels.fetchFailed'));
    } finally {
      refreshInFlightRef.current = false;
      setScanProvider('');
      setScanProgress(0);
      setRefreshing(false);
    }
  }, [showToast, t]);

  const addSelectedModel = useCallback(
    async (model: RouteModelInput) => {
      const id = model.internalId;
      const addition = routerMutationRef.current
        .catch(() => undefined)
        .then(async () => {
          // Retry a failed initial/refresh read before constructing a full-list
          // write. An unread configuration must never be treated as an empty one.
          if (!routerLoadedRef.current) await loadRouter();
          const candidateIds = [...new Set([...selectedIdsRef.current, id])];
          const router = await api.setSmartRouterCandidates(candidateIds);
          if (!router.candidateIds.includes(id)) {
            throw new Error(`Smart Router rejected candidate: ${id}`);
          }

          const nextIds = new Set(router.candidateIds);
          selectedIdsRef.current = nextIds;
          setSelectedIds(nextIds);
          setCustomModels((current) => [
            ...current.filter((entry) => entry.id !== id),
            {
              id,
              internalId: model.internalId,
              provider: model.name,
              modelId: model.modelId,
              baseUrl: model.baseUrl,
            },
          ]);
          setRouterBaseUrl(router.baseUrl.replace(/^https?:\/\//, ''));
        });
      routerMutationRef.current = addition;
      await addition;
    },
    [loadRouter]
  );

  const updateSelectedModel = useCallback(
    (model: RouteModelInput) => {
      routerMutationRef.current = routerMutationRef.current
        .catch(() => undefined)
        .then(async () => {
          setCustomModels((current) =>
            current.map((entry) =>
              entry.id === model.internalId
                ? { ...entry, provider: model.name, modelId: model.modelId, baseUrl: model.baseUrl }
                : entry
            )
          );
          await loadRouter();
        })
        .catch((error) => console.error('Refresh smart router after model update failed:', error));
    },
    [loadRouter]
  );

  const removeSelectedModel = useCallback(
    async (id: string) => {
      const removal = routerMutationRef.current
        .catch(() => undefined)
        .then(async () => {
          if (!routerLoadedRef.current) await loadRouter();
          let router = await api.removeSmartRouterCandidate(id);
          if (router.enabled && router.candidateIds.length === 0) {
            router = await api.setSmartRouterEnabled(false);
          }
          const nextIds = new Set(router.candidateIds);
          selectedIdsRef.current = nextIds;
          setSelectedIds(nextIds);
          setCustomModels((current) => current.filter((model) => model.id !== id));
          setRouterBaseUrl(router.baseUrl.replace(/^https?:\/\//, ''));
          setRouterEnabledState(router.enabled);
        });
      routerMutationRef.current = removal;
      try {
        await removal;
      } catch (error) {
        console.error('Remove smart router candidate failed:', error);
        showToast('error', t('error.requestFailed'));
      }
    },
    [loadRouter, showToast, t]
  );

  const reorderSelectedModel = useCallback(
    async (activeId: string, overId: string) => {
      const reorder = routerMutationRef.current
        .catch(() => undefined)
        .then(async () => {
          if (!routerLoadedRef.current) await loadRouter();
          const previous = selectedIdsRef.current;
          const ids = [...previous];
          const from = ids.indexOf(activeId);
          const to = ids.indexOf(overId);
          if (from < 0 || to < 0 || from === to) return;
          const candidateIds = arrayMove(ids, from, to);
          setSelectedIds(new Set(candidateIds));
          try {
            const router = await api.setSmartRouterCandidates(candidateIds);
            const next = new Set(router.candidateIds);
            selectedIdsRef.current = next;
            setSelectedIds(next);
          } catch (error) {
            setSelectedIds(previous);
            throw error;
          }
        });
      routerMutationRef.current = reorder;
      try {
        await reorder;
      } catch (error) {
        console.error('Save smart router order failed:', error);
        showToast('error', t('error.requestFailed'));
      }
    },
    [loadRouter, showToast, t]
  );

  return (
    <FreeModelsContext.Provider
      value={{
        catalog,
        models,
        customModels,
        selectedIds,
        refreshing,
        scanProvider,
        scanProgress,
        routerBaseUrl,
        routerEnabled,
        routerTogglePending: routerTogglePending || !routerLoaded,
        setRouterEnabled,
        refresh,
        addSelectedModel,
        updateSelectedModel,
        removeSelectedModel,
        reorderSelectedModel,
      }}
    >
      {children}
    </FreeModelsContext.Provider>
  );
}

export function FreeModelsTitleActions() {
  const { t } = useI18n();
  const [showHelp, setShowHelp] = useState(false);

  useEffect(() => {
    if (!showHelp) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setShowHelp(false);
    };
    window.addEventListener('keydown', closeOnEscape);
    return () => window.removeEventListener('keydown', closeOnEscape);
  }, [showHelp]);

  return (
    <>
      <button
        type="button"
        onClick={() => setShowHelp(true)}
        className="flex items-center gap-1.5 text-sm font-mono px-3 py-1.5 border border-cyber-border rounded-button text-cyber-text hover:bg-cyber-text/10 transition-colors"
      >
        <CircleHelp size={13} />
        {t('freeModels.help')}
      </button>

      {showHelp && (
        <div className="fixed inset-0 z-[9998] flex items-center justify-center">
          <div className="absolute inset-0 bg-black/60" onClick={() => setShowHelp(false)} />
          <div className="relative w-[520px] max-w-[90vw] border border-cyber-border/30 bg-cyber-surface shadow-2xl rounded-xl overflow-hidden">
            <div className="h-px w-full bg-cyber-border" />
            <button
              type="button"
              onClick={() => setShowHelp(false)}
              aria-label={t('btn.close')}
              className="absolute right-5 top-5 text-cyber-text-secondary hover:text-cyber-text transition-colors"
            >
              <X size={18} />
            </button>

            <div className="px-6 pt-6 pb-6 pr-14 space-y-4">
              <div className="flex gap-3">
                <Route size={18} className="mt-0.5 flex-shrink-0 text-cyber-accent" />
                <div>
                  <div className="text-sm font-semibold text-cyber-text">
                    {t('freeModels.help.routerTitle')}
                  </div>
                  <p className="mt-1 text-xs leading-5 text-cyber-text-secondary">
                    {t('freeModels.help.routerDesc')}
                  </p>
                </div>
              </div>

              <div className="h-px bg-cyber-border/60" />

              <div className="flex gap-3">
                <KeyRound size={18} className="mt-0.5 flex-shrink-0 text-cyber-accent" />
                <div>
                  <div className="text-sm font-semibold text-cyber-text">
                    {t('freeModels.help.useTitle')}
                  </div>
                  <ol className="mt-1 space-y-1.5 text-xs leading-5 text-cyber-text-secondary list-decimal list-inside">
                    <li>{t('freeModels.help.step1')}</li>
                    <li>{t('freeModels.help.step2')}</li>
                    <li>{t('freeModels.help.step3')}</li>
                  </ol>
                </div>
              </div>
            </div>

            <button
              type="button"
              onClick={() => setShowHelp(false)}
              className="w-full border-t border-cyber-border px-4 py-3 text-[14px] font-semibold text-cyber-text hover:bg-cyber-text/10 transition-colors"
            >
              {t('btn.close')}
            </button>
          </div>
        </div>
      )}
    </>
  );
}

interface RouteModelCardProps {
  model: RouteModelNode;
  priority: number;
  onEdit?: (id: string) => Promise<void>;
  onRemove: (id: string) => Promise<void>;
}

function RouteModelCard({
  model,
  priority,
  onEdit,
  onRemove,
  dragHandleProps,
  overlay = false,
}: RouteModelCardProps & {
  dragHandleProps?: ComponentPropsWithRef<'button'>;
  overlay?: boolean;
}) {
  const { t } = useI18n();
  const confirm = useConfirm();

  return (
    <div
      aria-hidden={overlay || undefined}
      className={`free-model-route-node relative rounded-lg ${overlay ? 'is-dragging' : ''}`}
    >
      <button
        {...dragHandleProps}
        type="button"
        tabIndex={overlay ? -1 : dragHandleProps?.tabIndex}
        aria-label={`${t('freeModels.router.priority')} ${priority}: ${model.modelId}`}
        className="relative w-full min-h-[72px] rounded-lg p-3 pr-9 text-left flex flex-col justify-center cursor-default touch-none focus-visible:outline focus-visible:outline-2 focus-visible:outline-cyber-accent"
      >
        <span className="free-model-route-priority absolute -left-2 -top-2 flex h-6 min-w-6 items-center justify-center rounded-full px-1 text-[11px] font-mono font-bold">
          {priority}
        </span>
        <span className="block w-full text-xs font-semibold text-cyber-text truncate">
          {shortModelName(model.modelId)}
        </span>
        <span className="block w-full mt-1 text-[10px] text-cyber-text-muted truncate">
          {model.provider}
        </span>
      </button>
      <div className="absolute right-1.5 inset-y-0 flex flex-col justify-center gap-1">
        {onEdit && (
          <button
            type="button"
            tabIndex={overlay ? -1 : undefined}
            onClick={() => void onEdit(model.id)}
            className="h-7 w-7 rounded-md flex items-center justify-center text-cyber-text-muted/60 hover:text-cyber-text hover:bg-cyber-text/10 transition-colors"
            aria-label={`${t('btn.edit')} ${shortModelName(model.modelId)}`}
          >
            <SquarePen size={14} strokeWidth={2.25} />
          </button>
        )}
        <button
          type="button"
          tabIndex={overlay ? -1 : undefined}
          onClick={async () => {
            const ok = await confirm({
              title: t('model.deleteTitle'),
              message: `${model.modelId} — ${t('freeModels.router.removeConfirm')}`,
              confirmText: t('btn.delete'),
              cancelText: t('btn.cancel'),
              type: 'danger',
            });
            if (ok) await onRemove(model.id);
          }}
          className="h-7 w-7 rounded-md flex items-center justify-center text-cyber-text-muted/60 hover:text-red-500 hover:bg-cyber-text/10 transition-colors"
          aria-label={`${t('btn.remove')} ${shortModelName(model.modelId)}`}
        >
          <Trash2 size={14} strokeWidth={2.25} />
        </button>
      </div>
    </div>
  );
}

function SortableRouteModel({
  registerNode,
  ...props
}: RouteModelCardProps & {
  registerNode: (id: string, node: HTMLDivElement | null) => void;
}) {
  const {
    attributes,
    listeners,
    setNodeRef,
    setActivatorNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: props.model.id });

  return (
    <div ref={(node) => registerNode(props.model.id, node)} className="min-w-0">
      <div
        ref={setNodeRef}
        style={{
          transform: CSS.Transform.toString(transform),
          transition,
          opacity: isDragging ? 0 : undefined,
        }}
      >
        <RouteModelCard
          {...props}
          dragHandleProps={{ ref: setActivatorNodeRef, ...attributes, ...listeners }}
        />
      </div>
    </div>
  );
}

export function FreeModelsMain() {
  const { t } = useI18n();
  const { showToast } = useToast();
  const {
    setNewModelForm,
    setEditingModelId,
    setModelModalDestination,
    setShowAddModelModal,
    setKeyDestroyed,
    setShowApiKey,
  } = useModelNexus();
  const {
    customModels,
    selectedIds,
    routerBaseUrl,
    routerEnabled,
    removeSelectedModel,
    reorderSelectedModel,
  } = useFreeModels();
  const editSelectedModel = async (id: string) => {
    try {
      const models = await api.getSmartRouterCandidates();
      const model = models.find((entry) => entry.internalId === id);
      if (!model) throw new Error(`Smart router model not found: ${id}`);
      const keyDestroyed = model.apiKey.startsWith('enc:v1:')
        ? await api.isKeyDestroyed(id)
        : false;
      setNewModelForm({
        name: model.name,
        baseUrl: model.baseUrl,
        anthropicUrl: model.anthropicUrl || '',
        apiKey: model.apiKey,
        modelId: model.modelId || '',
        apiProtocol: model.apiProtocol || '',
        responsesFallback: model.responsesFallback ?? false,
        autoDegradeProtocols: model.autoDegradeProtocols ?? false,
      });
      setKeyDestroyed(keyDestroyed);
      setShowApiKey(false);
      setEditingModelId(id);
      setModelModalDestination('freeRouter');
      setShowAddModelModal(true);
    } catch (error) {
      console.error('Open smart router model editor failed:', error);
      showToast('error', t('error.requestFailed'));
    }
  };
  const selectedModels = useMemo(() => {
    const modelsById = new Map(customModels.map((model) => [model.id, model]));
    return [...selectedIds].flatMap((id) => {
      const model = modelsById.get(id);
      return model ? [model] : [];
    });
  }, [customModels, selectedIds]);
  const [activeDragId, setActiveDragId] = useState<string | null>(null);
  const activeDragModel = selectedModels.find((model) => model.id === activeDragId);
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates })
  );
  const handleDragEnd = ({ active, over }: DragEndEvent) => {
    setActiveDragId(null);
    if (over && active.id !== over.id) {
      void reorderSelectedModel(String(active.id), String(over.id));
    }
  };
  const routerAnthropicBaseUrl = routerBaseUrl.replace(/\/v1\/?$/, '');
  const stageRef = useRef<HTMLDivElement | null>(null);
  const hubRef = useRef<HTMLDivElement | null>(null);
  const nodeRefs = useRef(new Map<string, HTMLDivElement>());
  const [routePaths, setRoutePaths] = useState<RoutePath[]>([]);
  const [activityPaths, setActivityPaths] = useState<RoutePath[]>([]);
  const [routeArrow, setRouteArrow] = useState<RouteArrow | null>(null);
  const [canvasSize, setCanvasSize] = useState({ width: 1, height: 1 });
  const [routerActivity, setRouterActivity] = useState<api.SmartRouterActivity>({
    candidateId: null,
    active: false,
    sequence: 0,
    updatedAtMs: 0,
  });
  const [activityObservedAtMs, setActivityObservedAtMs] = useState(0);

  useEffect(() => {
    let cancelled = false;
    if (!routerEnabled) return;
    let errorReported = false;
    const pollActivity = async () => {
      try {
        const activity = await api.getSmartRouterActivity();
        if (!cancelled) {
          setRouterActivity(activity);
          setActivityObservedAtMs(Date.now());
        }
      } catch (error) {
        if (!cancelled && !errorReported) {
          errorReported = true;
          console.error('Load smart router activity failed:', error);
        }
      }
    };
    void pollActivity();
    const timer = window.setInterval(() => void pollActivity(), ROUTER_ACTIVITY_POLL_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [routerEnabled]);

  const setNodeRef = useCallback((id: string, node: HTMLDivElement | null) => {
    if (node) nodeRefs.current.set(id, node);
    else nodeRefs.current.delete(id);
  }, []);

  const updatePaths = useCallback(() => {
    const stage = stageRef.current;
    const hub = hubRef.current;
    if (!stage || !hub) return;
    const stageBox = stage.getBoundingClientRect();
    const hubBox = hub.getBoundingClientRect();
    const endX = hubBox.left - stageBox.left + hubBox.width / 2;
    const endY = hubBox.bottom - stageBox.top;

    const entries = [...nodeRefs.current].map(([id, node]) => {
      const box = node.getBoundingClientRect();
      return {
        id,
        x: box.left - stageBox.left + box.width / 2,
        top: box.top - stageBox.top,
        bottom: box.bottom - stageBox.top,
      };
    });
    entries.sort((left, right) => left.top - right.top || left.x - right.x);

    const rows: (typeof entries)[] = [];
    for (const entry of entries) {
      const row = rows.find((candidate) => Math.abs(candidate[0].top - entry.top) < 12);
      if (row) row.push(entry);
      else rows.push([entry]);
    }

    const nextPaths: RoutePath[] = [];
    const nextActivityPaths: RoutePath[] = [];
    const parentById = new Map<string, (typeof entries)[number]>();
    const firstRow = rows[0];
    let nextArrow: RouteArrow | null = null;
    let routeBusY: number | null = null;
    let routeLineEndY: number | null = null;
    if (firstRow) {
      const arrowY = endY + HUB_ARROW_GAP;
      const lineEndY = arrowY + HUB_ARROW_HEIGHT + HUB_ARROW_LINE_GAP;
      routeLineEndY = lineEndY;
      if (firstRow.length === 1) {
        const entry = firstRow[0];
        nextPaths.push({
          id: entry.id,
          d: `M ${entry.x} ${entry.top - 3} V ${lineEndY}`,
        });
        nextArrow = { x: endX, y: arrowY };
      } else {
        const busY = endY + (firstRow[0].top - endY) / 2;
        routeBusY = busY;
        const firstX = firstRow[0].x;
        const lastX = firstRow[firstRow.length - 1].x;
        const busSegments = [];
        if (firstX < endX - 1) busSegments.push(`M ${firstX} ${busY} H ${endX}`);
        if (lastX > endX + 1) busSegments.push(`M ${lastX} ${busY} H ${endX}`);
        if (busSegments.length > 0) {
          nextPaths.push({
            id: 'hub-bus',
            d: busSegments.join(' '),
          });
        }
        nextPaths.push({
          id: 'hub-trunk',
          d: `M ${endX} ${busY} V ${lineEndY}`,
        });
        nextArrow = { x: endX, y: arrowY };
        firstRow.forEach((entry) => {
          nextPaths.push({
            id: entry.id,
            d: `M ${entry.x} ${entry.top - 3} V ${busY}`,
          });
        });
      }
    }

    rows.slice(1).forEach((row, rowIndex) => {
      const parentRow = rows[rowIndex];
      row.forEach((entry) => {
        const parent = parentRow.reduce((closest, candidate) =>
          Math.abs(candidate.x - entry.x) < Math.abs(closest.x - entry.x) ? candidate : closest
        );
        const bridgeY = parent.bottom + (entry.top - parent.bottom) / 2;
        parentById.set(entry.id, parent);
        nextPaths.push({
          id: entry.id,
          d:
            Math.abs(entry.x - parent.x) < 1
              ? `M ${entry.x} ${entry.top - 3} V ${parent.bottom + 3}`
              : `M ${entry.x} ${entry.top - 3} V ${bridgeY} H ${parent.x} V ${parent.bottom + 3}`,
        });
      });
    });

    if (firstRow && routeLineEndY !== null) {
      entries.forEach((entry) => {
        let current = entry;
        let d = `M ${entry.x} ${entry.top - 3}`;
        let parent = parentById.get(current.id);
        while (parent) {
          const bridgeY = parent.bottom + (current.top - parent.bottom) / 2;
          d +=
            Math.abs(current.x - parent.x) < 1
              ? ` V ${parent.bottom + 3}`
              : ` V ${bridgeY} H ${parent.x} V ${parent.bottom + 3}`;
          d += ` V ${parent.top - 3}`;
          current = parent;
          parent = parentById.get(current.id);
        }
        if (firstRow.length === 1) {
          d += ` V ${routeLineEndY}`;
        } else if (routeBusY !== null) {
          d += ` V ${routeBusY} H ${endX} V ${routeLineEndY}`;
        }
        nextActivityPaths.push({ id: entry.id, d });
      });
    }
    setCanvasSize({ width: stageBox.width, height: stageBox.height });
    setRoutePaths(nextPaths);
    setActivityPaths(nextActivityPaths);
    setRouteArrow(nextArrow);
  }, []);

  useLayoutEffect(() => {
    const frame = requestAnimationFrame(updatePaths);
    const stage = stageRef.current;
    if (!stage) return () => cancelAnimationFrame(frame);
    const observer = new ResizeObserver(updatePaths);
    observer.observe(stage);
    if (hubRef.current) observer.observe(hubRef.current);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [selectedModels, updatePaths]);

  const activityIsVisible =
    routerActivity.candidateId !== null &&
    (routerActivity.active ||
      activityObservedAtMs - routerActivity.updatedAtMs < ROUTER_ACTIVITY_GRACE_MS);
  const activeRoutePath = activityIsVisible
    ? activityPaths.find((path) => path.id === routerActivity.candidateId)
    : undefined;
  const routeRowCount = selectedModels.length === 0 ? 0 : Math.ceil(selectedModels.length / 4);
  const stageMinHeight = Math.max(
    550,
    24 +
      118 +
      HUB_TO_NODE_GAP +
      routeRowCount * 72 +
      Math.max(0, routeRowCount - 1) * NODE_ROW_GAP +
      NODE_ROW_GAP +
      32
  );

  return (
    <div className="free-model-router h-full min-h-[620px] px-2 py-1">
      <div
        ref={stageRef}
        className={`relative h-full pt-6 overflow-hidden ${routerEnabled ? '' : 'is-disabled'}`}
        style={{ minHeight: stageMinHeight }}
      >
        <svg
          className="absolute inset-0 z-0 h-full w-full pointer-events-none"
          viewBox={`0 0 ${canvasSize.width} ${canvasSize.height}`}
          preserveAspectRatio="none"
          aria-hidden="true"
        >
          {routePaths.map((path) => (
            <path key={path.id} d={path.d} className="free-model-route-path" />
          ))}
          {routerEnabled && activeRoutePath && (
            <path
              key={activeRoutePath.id}
              d={activeRoutePath.d}
              pathLength="1"
              className="free-model-route-glow"
            />
          )}
          {routeArrow && (
            <path
              d={`M ${routeArrow.x} ${routeArrow.y} L ${routeArrow.x - 6} ${routeArrow.y + HUB_ARROW_HEIGHT} H ${routeArrow.x + 6} Z`}
              className="free-model-route-arrow"
            />
          )}
        </svg>

        <div
          ref={hubRef}
          className="free-model-router-hub relative z-10 mx-auto w-max min-w-[270px] min-h-[118px] rounded-2xl flex flex-col items-center justify-center text-center px-4 py-3 cursor-default"
        >
          <div className="whitespace-nowrap text-xl font-semibold text-cyber-text">
            {t('freeModels.router.title')}
          </div>
          <div className="mt-3 space-y-1.5 text-center text-[10px] font-mono free-model-router-state">
            <div className="whitespace-nowrap">OpenAI : {routerBaseUrl}</div>
            <div className="whitespace-nowrap">Anthropic : {routerAnthropicBaseUrl}</div>
          </div>
        </div>

        {selectedModels.length === 0 ? (
          <div className="absolute inset-0 z-10 flex items-center justify-center text-center pointer-events-none">
            <div>
              <div className="font-medium text-sm text-cyber-text">
                {t('freeModels.router.emptyTitle')}
              </div>
              <div className="mt-2 text-xs text-cyber-text-muted">
                {t('freeModels.router.emptyDesc')}
              </div>
            </div>
          </div>
        ) : (
          <>
            <DndContext
              sensors={sensors}
              collisionDetection={closestCenter}
              onDragStart={({ active }) => setActiveDragId(String(active.id))}
              onDragEnd={handleDragEnd}
              onDragCancel={() => setActiveDragId(null)}
            >
              <SortableContext
                items={selectedModels.map((model) => model.id)}
                strategy={rectSortingStrategy}
              >
                <div
                  className="free-model-node-grid relative z-10 px-2"
                  style={{
                    gridTemplateColumns:
                      selectedModels.length >= 4
                        ? 'repeat(4, minmax(0, 1fr))'
                        : `repeat(${selectedModels.length}, minmax(140px, 190px))`,
                    columnGap: NODE_COLUMN_GAP,
                    rowGap: NODE_ROW_GAP,
                    marginTop: HUB_TO_NODE_GAP,
                  }}
                >
                  {selectedModels.map((model, index) => (
                    <SortableRouteModel
                      key={model.id}
                      model={model}
                      priority={index + 1}
                      registerNode={setNodeRef}
                      onEdit={model.id === 'local-server' ? undefined : editSelectedModel}
                      onRemove={removeSelectedModel}
                    />
                  ))}
                </div>
              </SortableContext>
              {createPortal(
                <DragOverlay
                  dropAnimation={null}
                  zIndex={1000}
                  className="pointer-events-none font-sans text-cyber-text"
                >
                  {activeDragModel && (
                    <RouteModelCard
                      model={activeDragModel}
                      priority={selectedModels.indexOf(activeDragModel) + 1}
                      onEdit={activeDragModel.id === 'local-server' ? undefined : editSelectedModel}
                      onRemove={removeSelectedModel}
                      overlay
                    />
                  )}
                </DragOverlay>,
                document.body
              )}
            </DndContext>
            <p
              className="text-center text-xs text-cyber-text-muted"
              style={{ marginTop: NODE_ROW_GAP }}
            >
              {t('freeModels.router.reorderHint')}
            </p>
          </>
        )}
      </div>
    </div>
  );
}

function FreeModelProviderRow({
  group,
  selected,
  onAdd,
}: {
  group: FreeModelProviderGroup;
  selected: boolean;
  onAdd: () => void;
}) {
  const { t } = useI18n();
  const iconSrc = getModelIcon(group.name, '');
  const hostname = (() => {
    try {
      return new URL(group.baseUrl).hostname;
    } catch {
      return group.baseUrl;
    }
  })();
  const openDocs = () => shellOpen(group.docsUrl).catch(() => window.open(group.docsUrl, '_blank'));

  return (
    <div className="free-model-provider-enter relative flex items-stretch rounded overflow-hidden bg-cyber-surface">
      <button
        type="button"
        onClick={onAdd}
        aria-label={`${t('freeModels.addToRouter')}: ${group.name}`}
        aria-pressed={selected}
        className="group/left flex-1 min-h-[64px] bg-gradient-to-r from-transparent to-transparent hover:from-cyber-text/15 hover:to-transparent transition-[background-image] duration-200"
      />
      <button
        type="button"
        onClick={openDocs}
        aria-label={`${t('freeModels.docs')}: ${group.name}`}
        className="group/right flex-1 min-h-[64px] bg-gradient-to-l from-transparent to-transparent hover:from-cyber-text/15 hover:to-transparent transition-[background-image] duration-200"
      />

      <div className="pointer-events-none absolute inset-0 flex items-center gap-3 px-3">
        {selected ? (
          <Check
            size={22}
            strokeWidth={2.5}
            className="flex-shrink-0 text-cyber-accent group-hover/left:scale-110 transition-all"
          />
        ) : (
          <Plus
            size={22}
            strokeWidth={2.5}
            className="flex-shrink-0 text-cyber-text-muted group-hover/left:text-cyber-text group-hover/left:scale-110 transition-all"
          />
        )}
        <div className="flex-shrink-0">
          {iconSrc ? (
            <img
              src={iconSrc}
              alt=""
              className="w-6 h-6"
              onError={(event) => {
                (event.target as HTMLImageElement).style.display = 'none';
              }}
            />
          ) : (
            <div className="w-6 h-6 flex items-center justify-center text-cyber-text">
              <Box size={22} />
            </div>
          )}
        </div>
        <div className="flex-1 min-w-0 flex flex-col justify-center">
          <div className="text-sm font-bold truncate leading-none">{group.name}</div>
          <div className="text-[10px] text-cyber-text-secondary truncate leading-tight mt-1 opacity-70">
            {hostname}
          </div>
        </div>
        <ExternalLink
          size={18}
          strokeWidth={2.25}
          className="flex-shrink-0 text-cyber-text-muted group-hover/right:text-cyber-text group-hover/right:scale-110 transition-all"
        />
      </div>
    </div>
  );
}

export function FreeModelsPanel() {
  const { t } = useI18n();
  const { showToast } = useToast();
  const confirm = useConfirm();
  const [activeTab, setActiveTab] = useState<'saved' | 'free'>('saved');
  const [addingId, setAddingId] = useState<string | null>(null);
  const setActivePage = useNavigationStore((state) => state.setActivePage);
  const {
    models,
    customModels,
    selectedIds,
    refreshing,
    scanProvider,
    scanProgress,
    refresh,
    addSelectedModel,
    removeSelectedModel,
  } = useFreeModels();
  const {
    userModels,
    isLoadingModels,
    modelUsageData,
    refreshingUsageIds,
    isRefreshingUsage,
    refreshSingleUsage,
    volcAkSkMissingIds,
    openAkskModal,
    handleCardEdit,
    handleCardDelete,
    setNewModelForm,
    setEditingModelId,
    setModelModalDestination,
    setShowAddModelModal,
    setKeyDestroyed,
    setShowApiKey,
  } = useModelNexus();
  const providerGroups = useMemo(() => {
    const groups = new Map<string, FreeModelProviderGroup>();
    models.forEach((model) => {
      const id = `${model.providerId}:${model.baseUrl}`;
      const group = groups.get(id);
      if (group) group.models.push(model);
      else {
        groups.set(id, {
          id,
          name: model.provider,
          baseUrl: model.baseUrl,
          docsUrl: model.docsUrl,
          models: [model],
        });
      }
    });
    return [...groups.values()].sort((left, right) => {
      return providerRank(left.models[0]) - providerRank(right.models[0]);
    });
  }, [models]);

  const openProvider = useCallback(
    (group: FreeModelProviderGroup) => {
      const modelIds = group.models.map((model) => model.modelId);
      setNewModelForm({
        name: group.name,
        baseUrl: group.baseUrl,
        anthropicUrl: '',
        apiKey: '',
        modelId: modelIds[0] ?? '',
        modelIdOptions: modelIds,
        apiProtocol: '',
        responsesFallback: false,
        autoDegradeProtocols: false,
      });
      setEditingModelId(null);
      setModelModalDestination('freeRouter');
      setShowAddModelModal(true);
    },
    [setEditingModelId, setModelModalDestination, setNewModelForm, setShowAddModelModal]
  );

  const openCustomModel = () => {
    setNewModelForm({
      name: '',
      baseUrl: '',
      anthropicUrl: '',
      apiKey: '',
      modelId: '',
      apiProtocol: '',
      responsesFallback: false,
      autoDegradeProtocols: false,
    });
    setEditingModelId(null);
    setKeyDestroyed(false);
    setShowApiKey(false);
    setModelModalDestination('freeRouter');
    setShowAddModelModal(true);
  };

  const addSavedModel = async (model: (typeof userModels)[number]) => {
    if (addingId || selectedIds.has(model.internalId)) return;
    if (selectedIds.size >= api.SMART_ROUTER_CANDIDATE_LIMIT) {
      showToast('warning', t('freeModels.router.limitReached'));
      return;
    }
    setAddingId(model.internalId);
    try {
      await addSelectedModel({
        internalId: model.internalId,
        name: model.name,
        baseUrl: model.baseUrl,
        modelId: model.modelId ?? '',
      });
    } catch (error) {
      console.error('Add saved model to smart router failed:', error);
      showToast('error', t('freeModels.saved.addFailed'));
    } finally {
      setAddingId(null);
    }
  };

  return (
    <div className="free-model-router h-full min-h-0 flex flex-col">
      <div className="h-10 shrink-0 px-2 mb-2 flex items-center justify-between gap-2">
        <div role="tablist" className="flex items-center gap-1">
          {(['saved', 'free'] as const).map((tab, index, tabs) => (
            <button
              key={tab}
              type="button"
              role="tab"
              id={`router-tab-${tab}`}
              aria-selected={activeTab === tab}
              aria-controls={`router-panel-${tab}`}
              tabIndex={activeTab === tab ? 0 : -1}
              onClick={() => setActiveTab(tab)}
              onKeyDown={(event) => {
                if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
                event.preventDefault();
                const next = event.key === 'Home' ? 0 : event.key === 'End' ? 1 : 1 - index;
                setActiveTab(tabs[next]);
                const tabButtons =
                  event.currentTarget.parentElement?.querySelectorAll<HTMLButtonElement>(
                    '[role="tab"]'
                  );
                tabButtons?.[next]?.focus();
              }}
              className={`px-3.5 py-2 text-[14px] font-semibold rounded transition-colors ${
                activeTab === tab
                  ? 'bg-cyber-elevated text-cyber-text'
                  : 'text-cyber-text-secondary hover:text-cyber-text hover:bg-cyber-elevated'
              }`}
            >
              {t(tab === 'saved' ? 'freeModels.tab.saved' : 'freeModels.tab.free')}
            </button>
          ))}
        </div>
        <button
          type="button"
          onClick={openCustomModel}
          aria-label={t('freeModels.customAdd')}
          className="w-9 h-9 shrink-0 flex items-center justify-center border border-cyber-border rounded-button text-cyber-text-secondary hover:text-cyber-text hover:bg-cyber-elevated transition-colors"
        >
          <Plus size={16} />
        </button>
      </div>
      <div
        role="tabpanel"
        id={`router-panel-${activeTab}`}
        aria-labelledby={`router-tab-${activeTab}`}
        className="flex-1 min-h-0 flex flex-col"
      >
        {activeTab === 'saved' ? (
          <div className="flex-1 p-2 overflow-y-auto">
            {isLoadingModels ? (
              <div className="h-full flex items-center justify-center" aria-busy="true">
                <RefreshCw size={18} className="animate-spin text-cyber-text-muted" />
              </div>
            ) : userModels.length === 0 ? (
              <div className="h-full flex flex-col items-center justify-center gap-3 text-xs">
                <p className="text-cyber-text-muted">{t('freeModels.saved.empty')}</p>
                <button
                  type="button"
                  onClick={() => setActivePage('models')}
                  className="text-cyber-accent hover:underline"
                >
                  {t('freeModels.saved.manage')}
                </button>
              </div>
            ) : (
              <div className="space-y-2">
                {userModels.map((model) => {
                  const selected = selectedIds.has(model.internalId);
                  const adding = addingId === model.internalId;
                  return (
                    <ModelListCard
                      key={model.internalId}
                      model={model}
                      onSelect={() => void addSavedModel(model)}
                      selectionDisabled={selected || Boolean(addingId)}
                      selectionLabel={`${t(selected ? 'freeModels.saved.added' : 'freeModels.addToRouter')}: ${model.name} — ${model.modelId ?? ''}`}
                      selection={
                        adding ? (
                          <RefreshCw
                            size={22}
                            className="shrink-0 animate-spin text-cyber-text-muted"
                          />
                        ) : selected ? (
                          <Check
                            size={22}
                            strokeWidth={2.5}
                            className="shrink-0 text-cyber-accent"
                          />
                        ) : (
                          <Plus
                            size={22}
                            strokeWidth={2.5}
                            className="shrink-0 text-cyber-text-muted"
                          />
                        )
                      }
                      usage={modelUsageData[model.internalId]}
                      refreshing={isRefreshingUsage || refreshingUsageIds.has(model.internalId)}
                      onRefreshUsage={(modelId) =>
                        volcAkSkMissingIds.has(modelId)
                          ? openAkskModal(modelId)
                          : refreshSingleUsage(modelId)
                      }
                      onEditModel={handleCardEdit}
                      onDeleteModel={async (modelId) => {
                        const ok = await confirm({
                          title: t('model.deleteTitle'),
                          message: t('model.deleteConfirm'),
                          confirmText: t('btn.delete'),
                          cancelText: t('btn.cancel'),
                          type: 'danger',
                        });
                        if (!ok) return;
                        try {
                          await handleCardDelete(modelId);
                          if (selectedIds.has(modelId)) await removeSelectedModel(modelId);
                        } catch (error) {
                          console.error('Delete saved router model failed:', error);
                          showToast('error', t('error.requestFailed'));
                        }
                      }}
                      t={t}
                    />
                  );
                })}
              </div>
            )}
          </div>
        ) : (
          <>
            <div className="h-10 shrink-0 px-2 flex items-center">
              <button
                type="button"
                onClick={() => void refresh()}
                disabled={refreshing}
                className={`flex-1 h-9 px-3 text-[14px] font-semibold border rounded-button transition-colors flex items-center justify-center gap-2 ${
                  !refreshing
                    ? 'border-cyber-border text-cyber-text-secondary hover:text-cyber-text hover:bg-cyber-elevated'
                    : 'border-cyber-border text-cyber-text-muted cursor-not-allowed'
                }`}
              >
                <RefreshCw size={14} className={refreshing ? 'animate-spin' : ''} />
                {t('freeModels.fetch')}
              </button>
            </div>
            {refreshing && (
              <div className="px-2 pb-2">
                <div className="h-1 rounded-full overflow-hidden bg-cyber-border/30">
                  <div
                    className="free-model-scan-progress h-full rounded-full bg-cyber-accent"
                    style={{ width: `${Math.max(scanProgress, 0.04) * 100}%` }}
                  />
                </div>
                <div className="mt-2 text-[11px] text-cyber-text-secondary truncate">
                  {scanProvider ? (
                    <>
                      {t('freeModels.scanning')}
                      {scanProvider}
                    </>
                  ) : (
                    <>
                      {t('freeModels.scanConnecting')}
                      <span className="free-model-scan-dots" aria-hidden="true">
                        <span>.</span>
                        <span>.</span>
                        <span>.</span>
                      </span>
                    </>
                  )}
                </div>
              </div>
            )}
            <div className="flex-1 p-2 overflow-y-auto">
              {providerGroups.length === 0 ? (
                <div className="h-full flex items-center justify-center">
                  {!refreshing && (
                    <div className="text-xs text-cyber-text-muted text-center">
                      {t('freeModels.fetchHint')}
                    </div>
                  )}
                </div>
              ) : (
                <div className="space-y-2">
                  {providerGroups.map((group) => (
                    <FreeModelProviderRow
                      key={group.id}
                      group={group}
                      selected={group.models.some((model) =>
                        customModels.some(
                          (routeModel) =>
                            selectedIds.has(routeModel.internalId) &&
                            routeModel.baseUrl === model.baseUrl &&
                            routeModel.modelId === model.modelId
                        )
                      )}
                      onAdd={() => openProvider(group)}
                    />
                  ))}
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
