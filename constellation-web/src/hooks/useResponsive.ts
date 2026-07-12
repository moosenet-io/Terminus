// SGUI-02: Responsive breakpoint detection
import { useState, useEffect } from 'react';

export type LayoutMode = 'full' | 'vertical' | 'mobile' | 'glanceable';

export function useResponsive(): LayoutMode {
  const [mode, setMode] = useState<LayoutMode>(() => getMode());

  useEffect(() => {
    const handler = () => setMode(getMode());
    window.addEventListener('resize', handler);
    return () => window.removeEventListener('resize', handler);
  }, []);

  return mode;
}

function getMode(): LayoutMode {
  const w = window.innerWidth;
  const h = window.innerHeight;
  if (w <= 480 && h > 800) return 'vertical'; // tall narrow: vertical monitor
  if (w <= 480) return 'mobile';
  if (w <= 768) return 'glanceable';
  return 'full';
}
