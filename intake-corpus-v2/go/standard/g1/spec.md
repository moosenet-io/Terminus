## Task
Create a fixed-capacity integer ring buffer in a new `ring.go` file in the
`wx-go` package.

## Description
Add a new file `ring.go` (package `wxgo`) defining a `RingBuffer` type that
stores up to a fixed number of `int` values, overwriting the oldest once full.

Public API:
- `func NewRingBuffer(capacity int) (*RingBuffer, error)` — constructor; return a
  non-nil error if `capacity <= 0`.
- `func (r *RingBuffer) Push(v int)` — append `v`; when already at capacity,
  overwrite the oldest element.
- `func (r *RingBuffer) ToSlice() []int` — current contents in insertion order
  (oldest first, newest last); length equals `Len()`.
- `func (r *RingBuffer) Len() int` — number of elements currently held.
- `func (r *RingBuffer) Cap() int` — the fixed capacity.

## FILES
- `ring.go` — the new file with the `RingBuffer` type and its methods.

## APPROACH
Back it with a `[]int` of length `capacity` plus a head index and a count. `Push`
writes at `(head + count) % capacity` while `count < capacity`, otherwise
overwrites at `head` and advances `head`. `ToSlice` walks `count` elements from
`head`.

## TEST PLAN
- `NewRingBuffer(3)`; push `1,2,3` → `ToSlice()` is `[1 2 3]`, `Len()` 3,
  `Cap()` 3.
- push `4` → `ToSlice()` is `[2 3 4]`, `Len()` still 3.
- A fresh buffer has `Len()` 0.
- `NewRingBuffer(0)` returns a non-nil error.
