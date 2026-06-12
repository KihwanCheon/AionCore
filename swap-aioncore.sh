#!/usr/bin/env bash
# aioncore 커스텀 빌드 ↔ 원본 스왑 도구
# 사용법:
#   ./swap-aioncore.sh apply     # 빌드한 바이너리로 교체 (AionUi 종료 후 실행)
#   ./swap-aioncore.sh rollback  # 원본으로 복구
#   ./swap-aioncore.sh status    # 현재 설치된 바이너리 버전/크기
#
# 전제: binaryResolver.ts 는 번들 경로를 existsSync 로만 검사(체크섬/서명 없음) →
#       파일을 덮어쓰면 그대로 사용됨. resolver 우선순위는 번들 > PATH 이므로
#       반드시 번들 파일을 교체해야 한다.

set -euo pipefail

BDIR="${LOCALAPPDATA}/Programs/AionUi/resources/bundled-aioncore/win32-x64"
LIVE="$BDIR/aioncore.exe"
BACKUP="$BDIR/aioncore.exe.orig-backup"
BUILT="$(cd "$(dirname "$0")" && pwd)/target/release/aioncore.exe"

is_locked() {
  # 라이브 파일에 쓰기를 시도해 잠금(앱 실행중) 여부 판정
  ( exec 9<>"$LIVE" ) 2>/dev/null && return 1 || return 0
}

case "${1:-status}" in
  apply)
    [ -f "$BUILT" ] || { echo "✗ 빌드 산출물 없음: $BUILT  (먼저 cargo build --release --bin aioncore)"; exit 1; }
    [ -f "$BACKUP" ] || cp "$LIVE" "$BACKUP"
    if is_locked; then echo "✗ aioncore.exe 가 잠겨 있습니다. AionUi 를 완전히 종료한 뒤 다시 실행하세요."; exit 1; fi
    cp "$BUILT" "$LIVE"
    echo "✓ 교체 완료. AionUi 재시작 후 새 aioncore 사용됨."
    "$LIVE" --version
    ;;
  rollback)
    [ -f "$BACKUP" ] || { echo "✗ 백업 없음: $BACKUP"; exit 1; }
    if is_locked; then echo "✗ aioncore.exe 가 잠겨 있습니다. AionUi 종료 후 재시도."; exit 1; fi
    cp "$BACKUP" "$LIVE"
    echo "✓ 원본 복구 완료."
    "$LIVE" --version
    ;;
  status)
    echo "live   : $("$LIVE" --version 2>/dev/null)  ($(stat -c%s "$LIVE" 2>/dev/null) bytes)"
    [ -f "$BACKUP" ] && echo "backup : 존재 ($(stat -c%s "$BACKUP") bytes)" || echo "backup : 없음"
    [ -f "$BUILT" ]  && echo "built  : $("$BUILT" --version 2>/dev/null)  ($(stat -c%s "$BUILT") bytes)" || echo "built  : 없음"
    is_locked && echo "lock   : 잠김(AionUi 실행중)" || echo "lock   : 해제(교체 가능)"
    ;;
  *) echo "사용법: $0 {apply|rollback|status}"; exit 1;;
esac
