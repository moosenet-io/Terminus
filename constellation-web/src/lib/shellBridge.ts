// CONST-25: a tiny bridge so palette commands registered at import time (`registerPanels.ts`,
// before any React tree exists) can (a) know the router's CURRENT path and (b) ask the Shell to
// re-poll health — without reaching for `window.location` (grep-gated to `aggregationClient.ts`
// only) or threading callbacks through every panel's props. Two independent, minimal seams:
//
//  - current path: App.tsx's Shell already has `useLocation()` (react-router, not
//    `window.location`) and calls `setCurrentPath` on every change; commands read it via
//    `getCurrentPath()`.
//  - refresh request: commands call `requestHealthRefresh()`, which dispatches a plain
//    `CustomEvent` on `window` (event dispatch/listen is NOT one of the gated APIs — GlobalBar's
//    keydown listener and App.tsx's resize listener already use `window.addEventListener` the
//    same way); Shell listens for it and calls its existing `fetchHealth()`.

let currentPath = '/';

/** Called by App.tsx's Shell on every route change (`useLocation().pathname`). */
export function setCurrentPath(path: string): void {
  currentPath = path;
}

/** Read by the "Copy current path" command — always up to date as of the last render. */
export function getCurrentPath(): string {
  return currentPath;
}

export const REFRESH_HEALTH_EVENT = 'constellation:refresh-health';

/** Called by the "Refresh health" command. */
export function requestHealthRefresh(): void {
  window.dispatchEvent(new CustomEvent(REFRESH_HEALTH_EVENT));
}
