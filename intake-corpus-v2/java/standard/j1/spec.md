## Task
Create a fixed-capacity integer ring buffer class `RingBuffer.java` in the
`wx-java` workspace.

## Description
Add a new class `RingBuffer` (default package) that stores up to a fixed number
of `int` values, overwriting the oldest value once it is full.

Public API:
- `RingBuffer(int capacity)` — constructor; throw `IllegalArgumentException` if
  `capacity <= 0`.
- `void push(int v)` — append `v`; when the buffer is already at capacity,
  overwrite the oldest element.
- `int[] toArray()` — return the current contents in insertion order
  (oldest first, newest last); length equals `size()`.
- `int size()` — number of elements currently held (never exceeds capacity).
- `int capacity()` — the fixed capacity.

## FILES
- `RingBuffer.java` — the new class.

## APPROACH
Back it with an `int[]` of length `capacity` plus a head index and a count.
`push` writes at `(head + count) % capacity` while `count < capacity`, otherwise
overwrites at `head` and advances `head`. `toArray` walks `count` elements from
`head`.

## TEST PLAN
- `new RingBuffer(3)`; push `1,2,3` → `toArray()` is `[1,2,3]`, `size()` 3,
  `capacity()` 3.
- push `4` → `toArray()` is `[2,3,4]`, `size()` still 3.
- A freshly constructed buffer has `size()` 0 and an empty `toArray()`.
- `new RingBuffer(0)` throws `IllegalArgumentException`.
