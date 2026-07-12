import { useLayoutEffect, useRef } from 'react';

interface TreeBranchProps {
  x1: number;
  y1: number;
  x2: number;
  y2: number;
  isNew?: boolean;
  status?: string;
  /** LIVE-04: When true, renders the branch in amber/held color. */
  held?: boolean;
}

export function TreeBranch({ x1, y1, x2, y2, isNew = false, status, held }: TreeBranchProps) {
  const pathRef = useRef<SVGPathElement>(null);
  const didAnimateRef = useRef(false);

  // useLayoutEffect runs before the browser paints, preventing the branch
  // from being visible for one frame before the grow animation starts.
  useLayoutEffect(() => {
    if (!isNew || didAnimateRef.current || !pathRef.current) return;
    if (document.hidden) return;

    const path = pathRef.current;
    const length = path.getTotalLength();

    path.style.setProperty('--branch-length', String(length));
    path.style.strokeDasharray = String(length);
    path.style.strokeDashoffset = String(length);
    didAnimateRef.current = true;
    path.classList.add('branch-growing');
  }, [isNew]);

  function handleAnimationEnd(e: React.AnimationEvent<SVGPathElement>) {
    if (e.animationName !== 'branch-grow') return;
    const path = pathRef.current;
    if (!path) return;
    path.classList.remove('branch-growing');
    path.style.strokeDasharray = '';
    path.style.strokeDashoffset = '';
  }

  const midY = (y1 + y2) / 2;
  const d = `M ${x1} ${y1} C ${x1} ${midY}, ${x2} ${midY}, ${x2} ${y2}`;

  const className = [
    'tree-branch',
    held ? 'tree-branch--held' : (status ? `tree-branch--${status}` : ''),
  ].filter(Boolean).join(' ');

  return (
    <path
      ref={pathRef}
      d={d}
      className={className}
      onAnimationEnd={handleAnimationEnd}
    />
  );
}
