# 내부 엔진 — walkdir 전환

2026-09-16 구현 및 아래 검증을 완료했습니다. 최종 UI 디자인 이전의 macOS 내부 기능이 범위입니다.

## 개발 범위

| 기능           | 구현 방향                                                                | 상태 |
| -------------- | ------------------------------------------------------------------------ | ---- |
| 폴더·파일 읽기 | walkdir 2.5.0으로 기존 macOS bulk 목록 파서 교체                         | 완료 |
| 전체 용량 집계 | 기존 단계별 집계기에 walkdir 목록 공급, 논리/할당량·하드링크 처리 재사용 | 완료 |
| 목록·하위 폴더 | 기존 페이지·캐시·지연 로딩과 연결                                        | 완료 |
| 작업 제어      | 진행 상태, 취소, 새로고침, 종료, 이전 결과 차단 유지                     | 완료 |
| 오류           | 권한 거부·이동·삭제·링크·부분 집계 처리                                  | 완료 |
| 오픈소스       | MIT 선택, 원문 고지·버전·출처와 앱 배포 포함                             | 완료 |
| 검증           | 임시 파일 기반 통합 회귀, 큰 폴더 실측, 프런트 빌드·E2E                  | 완료 |

## 구조와 선택

walkdir는 각 디렉터리의 직계 목록을 스트리밍합니다. 전체 트리는 기존 UsageScanner가 이를 이어서 순회하고, 폴더별 집계·취소·작업 스케줄링을 담당합니다. 라이브러리 교체와 무관한 앱 계약까지 다시 작성하지 않습니다. walkdir의 재귀 기능 전체로 기존 집계기를 대체하는 작업은 아닙니다.

폴더가 이동하면 폴더를 다시 선택해야 할 수 있습니다. 같은 경로의 다른 폴더를 기존 승인으로 자동 재승인하지 않습니다. 읽기 전용, 네이티브 선택, OS 권한, 심볼릭 링크 대상 미탐색, 마운트 경계는 유지합니다. 경로 기반 라이브러리의 동시 변경을 원자적 파일시스템 스냅샷으로 취급하지 않습니다.

기존 캐시·작업 수 제한은 유지하되, 모든 할당의 정확한 비용과 전체 앱 32/128 MiB 상한을 증명하는 것은 이번 완료 조건이 아닙니다. 기존 예약 수치는 앱 소유 데이터의 관리 지표이며 walkdir·표준 라이브러리·WebView의 실제 메모리 상한이 아닙니다.

## 완료 판정

실제 native runtime에서 목록·집계·취소·새로고침·종료 회귀가 통과하고, 큰 임시 폴더에서 처리 결과가 독립 계산과 일치하며, 필수 빌드/검사가 통과하면 내부 엔진 구현 완료로 판정합니다. 최종 조직도/마인드맵 디자인, Linux 제품 지원, 배포는 별도입니다.

## 구현 위치

- 목록 엔진: [macos.rs](../src-tauri/src/platform/macos.rs) — walkdir 커서와 native 승인·메타데이터.
- 용량 집계: [usage.rs](../src-tauri/src/scan/usage.rs), [macos_usage.rs](../src-tauri/src/platform/macos_usage.rs) — 기존 집계 로직에 새 커서 연결.
- 목록·페이지: [macos_listing.rs](../src-tauri/src/platform/macos_listing.rs), [listing/service.rs](../src-tauri/src/listing/service.rs).
- 작업·refresh·종료: [macos_runtime.rs](../src-tauri/src/platform/macos_runtime.rs), [scheduler](../src-tauri/src/scheduler/mod.rs).
- 이전 구현: [legacy/macOS bulk 원본](../src-tauri/legacy/macos_bulk.rs). 빌드에는 연결하지 않으며 원본을 보존합니다.
- 라이선스: [고지 원문](../licenses/THIRD-PARTY-NOTICES.txt). 앱 화면과 .app Resources/licenses에 포함합니다.

## 검증 결과 — 2026-09-16

- Rust 전체 테스트 167개 통과(과거 OS 특성 probe 5개 포함), 선택 실행 FD 회귀 1개 추가 통과. 디스크 이미지 mount 테스트 1개는 이번에 실행하지 않았습니다.
- Vitest 119개, Playwright 3개(탐색 흐름 및 라이선스 표시 포함) 통과.
- frontend build/lint/format, cargo check/fmt, macOS debug .app 빌드 통과.
- .app Resources/licenses에서 두 패키지의 COPYING/MIT 원문과 통합 고지 파일을 확인했습니다. MIT 원문은 Cargo에 설치된 잠금 버전에서 변경 없이 복사했습니다.
- 현재 Xcode는 라이선스 미동의 상태여서 기존 Command Line Tools를 DEVELOPER_DIR로 지정했습니다. 전역 개발 도구 설정이나 라이선스 동의 상태는 변경하지 않았습니다.

릴리스 native runtime, 각 구조 1회 실측입니다. 임시 fixture만 사용했고 결과는 별도 read_dir/symlink_metadata 기반 oracle과 비교했습니다. OS 캐시는 통제하지 않았으며 아래 시간은 UI/IPC를 제외합니다. 이전 엔진보다 빠르다는 비교 결과로 해석하지 않습니다.

| 구조          | 항목 수 | 첫 200개 목록 | 전체 집계 | 결과                             |
| ------------- | ------: | ------------: | --------: | -------------------------------- |
| 한 폴더       |  10,000 |        6.9 ms |    183 ms | 논리·할당량 일치                 |
| 한 폴더       | 100,000 |       10.4 ms |  2,112 ms | 논리·할당량 일치                 |
| 넓은 트리     | 101,000 |       12.0 ms |  2,008 ms | 논리·할당량 일치                 |
| 하드링크 포함 |  10,200 |       12.1 ms |    233 ms | 중복 제외 후 일치                |
| 깊이 256      |     256 |     해당 없음 |   20.6 ms | 기존 깊이 128 제한으로 부분 결과 |

5개 모두 실행 중 취소 접수·작업 종료·세션 닫기 후 FD 원상복귀를 확인했습니다. [집계 지표 원본](validation/walkdir-native-baseline.jsonl).

## 실용적 제한과 후속 범위

- walkdir를 디렉터리별로 사용하므로 OS ReadDir 핸들이 승인 핸들 외에 존재합니다. 실제 FD를 별도로 측정했고 종료 후 반환을 검증했습니다. 내부 permit 수를 OS FD 수라고 주장하지 않습니다.
- 기존 깊이·캐시·작업 제한에 도달하면 부분 결과를 표시합니다. 전체 트리를 무제한 메모리에 보관하지 않습니다. 기존 바이트 예약 코드도 보존하지만 전체 앱 메모리 상한 보증으로 취급하지 않습니다.
- 이번 native runtime 테스트는 실제 파일 접근·집계·refresh·재선택을 수행합니다. 네이티브 picker/WebView의 수동 전체 여정은 이번에 재실행하지 않았습니다. 브라우저 E2E는 fixture adapter를 사용합니다.
- 최종 UI·UX, Linux 제품 실행, 배포는 별도입니다. 기존 미커밋 IPC/프런트 수정과 package-lock.json 변경은 보존했습니다.
