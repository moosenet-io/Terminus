// CONST-24: ambient module shim for `@nivo/parallel-coordinates`.
//
// The installed 0.99.0 package declares `"types": "./dist/types/index.d.ts"` in its
// package.json but does NOT ship that directory (only the `.cjs.js`/`.mjs` bundles are
// present — verified via `find node_modules/@nivo/parallel-coordinates/dist`). Every sibling
// pinned nivo package (`@nivo/boxplot`, `@nivo/swarmplot`, `@nivo/radar`, `@nivo/heatmap`,
// `@nivo/scatterplot`) ships real types; this one specific package's published dist is
// missing them. Rather than block on an upstream/registry fix, this shim declares just
// enough of the runtime-verified export surface (`node -e "console.log(Object.keys(require(
// '@nivo/parallel-coordinates')))"` confirms `ResponsiveParallelCoordinates` exists at
// runtime) for `ParallelCoordinatesChart.tsx` to typecheck. Loose `any`-shaped props are
// deliberate here — this is the one sanctioned "the library's own types are broken" escape
// hatch, not a general license to under-type the wrapper itself (the wrapper's own exported
// props are still fully typed).
declare module '@nivo/parallel-coordinates' {
  import type { ComponentType, ReactNode } from 'react';

  export interface ParallelCoordinatesVariable {
    id: string;
    value: string;
    min?: number | 'auto';
    max?: number | 'auto';
    label?: ReactNode;
    legendOffset?: number;
    tickValues?: number[];
    tickFormat?: (value: number) => string | number;
  }

  export interface ParallelCoordinatesCustomLayerContext {
    computedData: Array<{
      id: string;
      index: number;
      group?: string;
      color: string;
      data: Record<string, unknown>;
      points: [number, number][];
    }>;
    variables: ParallelCoordinatesVariable[];
    lineGenerator: (points: [number, number][]) => string | null;
  }

  export type ParallelCoordinatesLayer =
    | 'axes'
    | 'lines'
    | 'legends'
    | ((ctx: ParallelCoordinatesCustomLayerContext) => ReactNode);

  export interface ResponsiveParallelCoordinatesProps {
    data: Record<string, unknown>[];
    variables: ParallelCoordinatesVariable[];
    groupBy?: string;
    groups?: { id: string; label?: string }[];
    layout?: 'horizontal' | 'vertical';
    margin?: { top?: number; right?: number; bottom?: number; left?: number };
    colors?: (d: { id: string; label?: string }) => string;
    lineWidth?: number;
    lineOpacity?: number;
    layers?: ParallelCoordinatesLayer[];
    isInteractive?: boolean;
    animate?: boolean;
    theme?: Record<string, unknown>;
  }

  export const ResponsiveParallelCoordinates: ComponentType<ResponsiveParallelCoordinatesProps>;
}
