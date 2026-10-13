import { useApp } from "../store/app";

/**
 * VS Code-style Go Back / Go Forward across tabs: Alt+Left / Alt+Right and the mouse back / forward
 * buttons. Tab switches are recorded automatically; editors call `navRecord()` before jumping far
 * inside themselves and register a site so their caret position can be captured and restored.
 */
export interface NavLoc {
  connKey: string;
  tabId: string;
  part?: string;
  line?: number;
  column?: number;
}

type Spot = Pick<NavLoc, "part" | "line" | "column">;

interface Site {
  here(): Spot | null;
  restore(loc: Spot): void;
}

const sites = new Map<string, Site>();
const back: NavLoc[] = [];
const fwd: NavLoc[] = [];
const LIMIT = 100;
let stepping = false;

/** Lets back / forward read and restore the caret of the editor shown in `tabId`. */
export function registerNavSite(tabId: string, site: Site): () => void {
  sites.set(tabId, site);
  return () => {
    if (sites.get(tabId) === site) sites.delete(tabId);
  };
}

const locOf = (connKey: string, tabId: string): NavLoc => ({ connKey, tabId, ...(sites.get(tabId)?.here() ?? {}) });

function current(): NavLoc | null {
  const s = useApp.getState();
  const connKey = s.activeConn;
  const tabId = connKey ? s.activeTab[connKey] : undefined;
  return connKey && tabId ? locOf(connKey, tabId) : null;
}

const same = (a: NavLoc, b: NavLoc) => a.tabId === b.tabId && a.part === b.part && a.line === b.line;

function push(loc: NavLoc) {
  const top = back[back.length - 1];
  if (!top || !same(top, loc)) back.push(loc);
  if (back.length > LIMIT) back.shift();
  fwd.length = 0;
}

/** Remember where the caret is now, before a jump. */
export function navRecord() {
  const c = current();
  if (c) push(c);
}

function step(from: NavLoc[], to: NavLoc[]) {
  const s = useApp.getState();
  while (from.length) {
    const loc = from.pop()!;
    if (!s.tabs.some((t) => t.id === loc.tabId)) continue; // tab was closed
    const here = current();
    if (here) to.push(here);
    stepping = true;
    try {
      s.setActiveConn(loc.connKey);
      s.setActiveTab(loc.connKey, loc.tabId);
    } finally {
      stepping = false;
    }
    // Let a hidden tab become visible (and its editor lay out) before moving the caret.
    setTimeout(() => sites.get(loc.tabId)?.restore(loc), 0);
    return;
  }
}

export const navBack = () => step(back, fwd);
export const navForward = () => step(fwd, back);

// Record every tab switch (Explorer, Ctrl+click, tab strip…) except our own back / forward moves.
let last = current();
useApp.subscribe(() => {
  const s = useApp.getState();
  const connKey = s.activeConn;
  const tabId = connKey ? s.activeTab[connKey] : undefined;
  if (last && (last.connKey !== connKey || last.tabId !== tabId) && !stepping) {
    // The previous tab's editor is still mounted, so its caret can be read now.
    const prev = s.tabs.some((t) => t.id === last!.tabId) ? locOf(last.connKey, last.tabId) : null;
    if (prev) push(prev);
  }
  last = connKey && tabId ? { connKey, tabId } : null;
});

/** Global Alt+Left / Alt+Right and mouse buttons 3 / 4. Capture phase so Monaco never sees them. */
export function installNavKeys(): () => void {
  const onKey = (e: KeyboardEvent) => {
    if (!e.altKey || e.ctrlKey || e.shiftKey || e.metaKey) return;
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "ArrowLeft") navBack();
    else navForward();
  };
  const swallow = (e: MouseEvent) => {
    if (e.button === 3 || e.button === 4) e.preventDefault();
  };
  const onMouse = (e: MouseEvent) => {
    if (e.button !== 3 && e.button !== 4) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.button === 3) navBack();
    else navForward();
  };
  window.addEventListener("keydown", onKey, true);
  window.addEventListener("mousedown", swallow, true);
  window.addEventListener("mouseup", onMouse, true);
  return () => {
    window.removeEventListener("keydown", onKey, true);
    window.removeEventListener("mousedown", swallow, true);
    window.removeEventListener("mouseup", onMouse, true);
  };
}
