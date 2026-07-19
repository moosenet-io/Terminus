// CONST-28: unit tests for the fleet ring buffer — the two properties the spec calls out
// explicitly ("cap 120, transition detection"), plus the "keep last-known content on a
// failing poll" edge case.
import { describe, expect, it } from 'vitest';
import type { HealthStatus } from '../../lib/aggregationClient';
import {
  RING_BUFFER_CAPACITY,
  emptyFleetRingBuffers,
  pushHealthPoll,
  transitions,
  uptimeRatio,
} from './fleetRingBuffer';

function health(system: HealthStatus['system'], available: boolean): HealthStatus[] {
  return [{ system, available }];
}

describe('pushHealthPoll', () => {
  it('accumulates samples for a system in poll order', () => {
    let buffers = emptyFleetRingBuffers();
    buffers = pushHealthPoll(buffers, health('chord', true), 1000);
    buffers = pushHealthPoll(buffers, health('chord', true), 2000);
    buffers = pushHealthPoll(buffers, health('chord', false), 3000);
    expect(buffers.chord).toEqual([
      { t: 1000, available: true },
      { t: 2000, available: true },
      { t: 3000, available: false },
    ]);
  });

  it('caps at RING_BUFFER_CAPACITY (120), dropping the oldest sample once full', () => {
    let buffers = emptyFleetRingBuffers();
    for (let i = 0; i < RING_BUFFER_CAPACITY + 10; i++) {
      buffers = pushHealthPoll(buffers, health('harmony', true), i);
    }
    expect(buffers.harmony).toHaveLength(RING_BUFFER_CAPACITY);
    // Oldest 10 samples (t=0..9) should have fallen off the front.
    expect(buffers.harmony![0].t).toBe(10);
    expect(buffers.harmony![RING_BUFFER_CAPACITY - 1].t).toBe(RING_BUFFER_CAPACITY + 9);
  });

  it('never exceeds capacity even one sample at a time past the cap', () => {
    let buffers = emptyFleetRingBuffers();
    for (let i = 0; i < 500; i++) {
      buffers = pushHealthPoll(buffers, health('lumina', i % 2 === 0), i);
      expect(buffers.lumina!.length).toBeLessThanOrEqual(RING_BUFFER_CAPACITY);
    }
  });

  it('leaves a system untouched when a poll omits it (failing-poll edge case)', () => {
    let buffers = emptyFleetRingBuffers();
    buffers = pushHealthPoll(buffers, health('terminus', true), 1000);
    // Simulate a poll that only reports other systems (this system's probe timed out/omitted).
    buffers = pushHealthPoll(buffers, health('chord', true), 2000);
    expect(buffers.terminus).toEqual([{ t: 1000, available: true }]);
  });

  it('does not mutate the buffers object passed in', () => {
    const before = emptyFleetRingBuffers();
    const after = pushHealthPoll(before, health('chord', true), 1000);
    expect(before).toEqual({});
    expect(after).not.toBe(before);
  });
});

describe('transitions', () => {
  it('reports the first sample as a transition from null', () => {
    const buffers = pushHealthPoll(emptyFleetRingBuffers(), health('chord', true), 1000);
    expect(transitions(buffers, 'chord')).toEqual([{ system: 'chord', from: null, to: true, t: 1000 }]);
  });

  it('detects a flap (true -> false -> true) and ignores repeated identical polls', () => {
    let buffers = emptyFleetRingBuffers();
    buffers = pushHealthPoll(buffers, health('chord', true), 1000);
    buffers = pushHealthPoll(buffers, health('chord', true), 2000); // no change
    buffers = pushHealthPoll(buffers, health('chord', false), 3000); // down
    buffers = pushHealthPoll(buffers, health('chord', false), 4000); // no change
    buffers = pushHealthPoll(buffers, health('chord', true), 5000); // back up

    expect(transitions(buffers, 'chord')).toEqual([
      { system: 'chord', from: null, to: true, t: 1000 },
      { system: 'chord', from: true, to: false, t: 3000 },
      { system: 'chord', from: false, to: true, t: 5000 },
    ]);
  });

  it('returns an empty array for a system with no samples', () => {
    expect(transitions(emptyFleetRingBuffers(), 'lumina')).toEqual([]);
  });
});

describe('uptimeRatio', () => {
  it('returns null for an empty buffer', () => {
    expect(uptimeRatio(emptyFleetRingBuffers(), 'chord')).toBeNull();
  });

  it('computes the fraction of available polls', () => {
    let buffers = emptyFleetRingBuffers();
    buffers = pushHealthPoll(buffers, health('chord', true), 1);
    buffers = pushHealthPoll(buffers, health('chord', true), 2);
    buffers = pushHealthPoll(buffers, health('chord', false), 3);
    buffers = pushHealthPoll(buffers, health('chord', true), 4);
    expect(uptimeRatio(buffers, 'chord')).toBe(0.75);
  });
});
