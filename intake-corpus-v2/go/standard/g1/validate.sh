#!/usr/bin/env bash
# Validator for go-standard-g1 (new wxgo.RingBuffer). Stage contract per README.
set -uo pipefail
WORK="${MINT_WORK:-$PWD}"
cd "$WORK" || { echo "STAGE:COMPILE fail"; echo "FAIL"; exit 1; }

command -v go >/dev/null 2>&1 || { echo "TOOLCHAIN:missing go"; exit 3; }

export GOCACHE="${GOCACHE:-$WORK/.gocache}"
export GOPATH="${GOPATH:-$WORK/.gopath}"
export GOFLAGS="-mod=mod"
export GOPROXY=off
export GO111MODULE=on

if go build ./... >/tmp/mint_gb.$$ 2>&1; then
  echo "STAGE:COMPILE ok"
else
  echo "STAGE:COMPILE fail"; cat /tmp/mint_gb.$$ >&2; echo "FAIL"; exit 1
fi

if go test ./... >/tmp/mint_gt.$$ 2>&1; then
  echo "STAGE:TESTS ok"
else
  echo "STAGE:TESTS fail"; cat /tmp/mint_gt.$$ >&2
fi

cat > "$WORK/mint_change_test.go" <<'GO'
package wxgo

import (
	"reflect"
	"testing"
)

func TestMintChange(t *testing.T) {
	rb, err := NewRingBuffer(3)
	if err != nil {
		t.Fatalf("NewRingBuffer(3): %v", err)
	}
	if rb.Len() != 0 || rb.Cap() != 3 {
		t.Fatalf("fresh Len=%d Cap=%d", rb.Len(), rb.Cap())
	}
	rb.Push(1)
	rb.Push(2)
	rb.Push(3)
	if !reflect.DeepEqual(rb.ToSlice(), []int{1, 2, 3}) || rb.Len() != 3 {
		t.Fatalf("filled: %v len %d", rb.ToSlice(), rb.Len())
	}
	rb.Push(4)
	if !reflect.DeepEqual(rb.ToSlice(), []int{2, 3, 4}) || rb.Len() != 3 {
		t.Fatalf("overwrite: %v len %d", rb.ToSlice(), rb.Len())
	}
	if _, err := NewRingBuffer(0); err == nil {
		t.Fatalf("want error for capacity 0")
	}
}
GO
if go test -run TestMintChange ./... >/tmp/mint_gk.$$ 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; rm -f "$WORK/mint_change_test.go"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_gk.$$ >&2; echo "FAIL"; rm -f "$WORK/mint_change_test.go"; exit 1
fi
