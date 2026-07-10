#!/usr/bin/env bash
# Validator for go-blitz-g1 (wxgo.ValidateEmail). Stage contract per README:
# STAGE:COMPILE / STAGE:TESTS / STAGE:CHANGE, TOOLCHAIN:missing.
set -uo pipefail
WORK="${MINT_WORK:-$PWD}"
cd "$WORK" || { echo "STAGE:COMPILE fail"; echo "FAIL"; exit 1; }

command -v go >/dev/null 2>&1 || { echo "TOOLCHAIN:missing go"; exit 3; }

# Keep the build/module cache inside the staged temp dir; run fully offline
# (the workspace has no external dependencies).
export GOCACHE="${GOCACHE:-$WORK/.gocache}"
export GOPATH="${GOPATH:-$WORK/.gopath}"
export GOFLAGS="-mod=mod"
export GOPROXY=off
export GO111MODULE=on

# ---- COMPILE ----
if go build ./... >/tmp/mint_gb.$$ 2>&1; then
  echo "STAGE:COMPILE ok"
else
  echo "STAGE:COMPILE fail"; cat /tmp/mint_gb.$$ >&2; echo "FAIL"; exit 1
fi

# ---- TESTS: baseline (+ any model-authored *_test.go) ----
if go test ./... >/tmp/mint_gt.$$ 2>&1; then
  echo "STAGE:TESTS ok"
else
  echo "STAGE:TESTS fail"; cat /tmp/mint_gt.$$ >&2
fi

# ---- CHANGE: hidden independent behavior check ----
cat > "$WORK/mint_change_test.go" <<'GO'
package wxgo

import "testing"

func TestMintChange(t *testing.T) {
	got, err := ValidateEmail("<email>") // pii-test-fixture
	if err != nil || got != "<email>" { // pii-test-fixture
		t.Fatalf("normalize: got %q err %v", got, err)
	}
	for _, bad := range []string{"noatsign", "a@@b.com", "@example.com", "user@examplecom", "a <email>"} { // pii-test-fixture
		if _, err := ValidateEmail(bad); err == nil {
			t.Fatalf("want error for %q", bad)
		}
	}
}
GO
if go test -run TestMintChange ./... >/tmp/mint_gk.$$ 2>&1; then
  echo "STAGE:CHANGE ok"; echo "PASS"; rm -f "$WORK/mint_change_test.go"; exit 0
else
  echo "STAGE:CHANGE fail"; cat /tmp/mint_gk.$$ >&2; echo "FAIL"; rm -f "$WORK/mint_change_test.go"; exit 1
fi
