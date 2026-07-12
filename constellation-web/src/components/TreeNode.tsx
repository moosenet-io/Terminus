// LIVE-04: Added trunk label, item ID badge, stage nodes (P/E/T/C/R/M), HELD badge, elapsed time.
import { useEffect, useRef, useState } from 'react';
import type { TreeStage } from '../types/tree';

export type TaskStatus = 'pending' | 'active' | 'complete' | 'done' | 'failed';

const STATUS_ICON: Record<TaskStatus, string> = {
  pending: '○',
  active: '◉',
  complete: '✓',
  done: '✓',
  failed: '✗',
};

const ANIM_NAME_TO_CLASS: Record<string, string> = {
  'node-bloom': 'node-blooming',
  'node-pop': 'node-completing',
  'flourish-glow': 'node-flourishing',
};

/** LIVE-04: One-letter abbreviations for the 6 pipeline stages. */
const STAGE_ABBR: Record<string, string> = {
  plan: 'P',
  execute: 'E',
  test: 'T',
  check: 'C',
  review: 'R',
  merge: 'M',
};

/** Format elapsed seconds as "Xm" or "Xs". */
function formatElapsed(secs: number): string {
  if (secs <= 0) return '';
  if (secs >= 60) return `${Math.floor(secs / 60)}m`;
  return `${secs}s`;
}

interface TreeNodeProps {
  id: string;
  label: string;
  status: TaskStatus;
  x: number;
  y: number;
  staggerDelay?: number;
  specComplete?: boolean;
  /** LIVE-04: True when this node is a spec root (trunk). */
  isSpec?: boolean;
  /** LIVE-04: Spec description shown on trunk nodes (truncated to 40 chars). */
  specTitle?: string;
  /** LIVE-04: Item ID badge text, e.g. "LM-42". */
  itemId?: string;
  /** LIVE-04: Pipeline stages for this item. */
  stages?: TreeStage[];
  /** LIVE-04: True when this task is on hold. */
  held?: boolean;
  /** LIVE-04: Triage step string, e.g. "2/5". */
  triageStep?: string;
  /** LIVE-04: Elapsed seconds for active worker. */
  elapsed_secs?: number;
}

export function TreeNode({
  id,
  label,
  status,
  x,
  y,
  staggerDelay = 0,
  specComplete = false,
  isSpec = false,
  specTitle,
  itemId,
  stages,
  held = false,
  triageStep,
  elapsed_secs = 0,
}: TreeNodeProps) {
  const prevStatusRef = useRef<TaskStatus>(status);
  const flourishFiredRef = useRef(false);
  const [animClass, setAnimClass] = useState('');

  useEffect(() => {
    const prev = prevStatusRef.current;
    if (prev === status) return;

    if (!document.hidden) {
      if (prev === 'pending' && status === 'active') {
        setAnimClass('node-blooming');
      } else if (
        (prev === 'active' || prev === 'pending') &&
        (status === 'complete' || status === 'done')
      ) {
        setAnimClass('node-completing');
      }
    }

    prevStatusRef.current = status;
  }, [status]);

  useEffect(() => {
    if (!specComplete || flourishFiredRef.current) return;
    if (document.hidden) return;

    const timer = setTimeout(() => {
      flourishFiredRef.current = true;
      setAnimClass('node-flourishing');
    }, staggerDelay);

    return () => clearTimeout(timer);
  }, [specComplete, staggerDelay]);

  function handleAnimationEnd(e: React.AnimationEvent<HTMLDivElement>) {
    const expectedClass = ANIM_NAME_TO_CLASS[e.animationName];
    if (expectedClass) {
      setAnimClass((current) => (current === expectedClass ? '' : current));
    }
  }

  const truncatedSpecTitle = specTitle ? specTitle.slice(0, 40) : '';
  const elapsed = formatElapsed(elapsed_secs);

  return (
    <div
      id={id}
      className={`tree-node tree-node--${status}${isSpec ? ' tree-node--spec' : ''}${held ? ' tree-node--held' : ''}${animClass ? ` ${animClass}` : ''}`}
      style={{ left: x, top: y }}
      onAnimationEnd={handleAnimationEnd}
      aria-label={`${label}: ${status}`}
    >
      {/* Main node circle */}
      <div className="tree-node__circle">{STATUS_ICON[status]}</div>

      {/* LIVE-04: Spec trunk label — spec_id · spec_title */}
      {isSpec && truncatedSpecTitle && (
        <div className="tree-node__trunk-label">
          <span className="tree-node__trunk-id">{id}</span>
          <span className="tree-node__trunk-sep"> · </span>
          <span className="tree-node__trunk-title">{truncatedSpecTitle}</span>
        </div>
      )}

      {/* LIVE-04: Branch info row — item ID badge + HELD badge + elapsed */}
      {!isSpec && (
        <div className="tree-node__branch-info">
          {itemId && (
            <span className={`tree-node__item-id${held ? ' tree-node__item-id--held' : ''}`}>
              {itemId}
            </span>
          )}
          {held && (
            <span className="tree-node__held-badge">
              {triageStep ? `TRIAGE ${triageStep}` : 'HELD'}
            </span>
          )}
          {elapsed && status === 'active' && (
            <span className="tree-node__elapsed">{elapsed}</span>
          )}
        </div>
      )}

      {/* Item title / spec label */}
      <span className="tree-node__label" title={label}>
        {label.slice(0, 30)}
      </span>

      {/* LIVE-04: Pipeline stage nodes — P E T C R M */}
      {!isSpec && stages && stages.length > 0 && (
        <div className="tree-node__stages" aria-label="Pipeline stages">
          {stages.map((stage) => {
            const abbr = STAGE_ABBR[stage.name] ?? stage.name.slice(0, 1).toUpperCase();
            return (
              <div
                key={stage.name}
                className={`tree-stage tree-stage--${stage.status}`}
                title={`${stage.name}: ${stage.status}`}
                aria-label={`${stage.name}: ${stage.status}`}
              >
                {abbr}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
