// CGUI-02 (TERM #525): reusable panel scroll frame.
//
// Root-cause fix for the operator complaint "the pages don't scroll": the app shell
// (App.tsx) fixes the viewport at height:100vh; overflow:hidden and the routed panel
// container sets NO overflow-y, so a panel only scrolls if ITS OWN root declares the
// scroll. The ported `pages/*` panels do (`height:100%; overflow-y:auto`); the Terminus
// module panels + Engine Diagram did not, so their content was clipped with no scrollbar.
//
// <PanelRoot> standardises that scroll frame in one place:
//   - height:100% + min-height:0  → the flex child can shrink below its content height
//     (the `min-height:0` is the load-bearing mechanism — without it a flex child refuses
//     to scroll and overflows its parent, the classic "won't scroll" bug, guide-spec §5).
//   - overflow-y:auto             → content taller than the frame scrolls; shorter doesn't.
//   - className `hf-scroll`        → the DS violet custom scrollbar (globals.css).
//
// The shell (global bar + module rail) stays a fixed frame; only the canvas inside a
// PanelRoot scrolls. Layout (padding / flex column / gap) is passed through via `style`,
// so a panel's existing root styling moves onto the scroll frame unchanged rather than
// nesting a second scroll container.
import type { CSSProperties, ReactNode } from 'react';

export interface PanelRootProps {
  children: ReactNode;
  /** Extra classes appended after the always-present `hf-scroll`. */
  className?: string;
  /** Panel layout (padding, display, gap, …). Merged after the scroll-frame defaults;
   *  callers should NOT override height/minHeight/overflowY unless they truly intend to. */
  style?: CSSProperties;
}

export function PanelRoot({ children, className, style }: PanelRootProps) {
  return (
    <div
      className={className ? `hf-scroll ${className}` : 'hf-scroll'}
      style={{ height: '100%', minHeight: 0, overflowY: 'auto', ...style }}
    >
      {children}
    </div>
  );
}
