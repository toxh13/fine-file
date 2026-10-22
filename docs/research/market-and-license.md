# file-atlas 시장·라이선스 조사

- 조사일: 2026-10-21
- 방법: 공식 제품 사이트, 변경 기록, EULA, 이용약관, GitHub 저장소와 LICENSE 파일, crates.io, Microsoft Learn, 법령 조문을 직접 확인했다. 2차 기사는 단서로만 썼고, 2차 출처뿐인 내용은 "(2차)"로 표시했다.
- 표의 `[ID]`는 맨 아래 출처 목록의 링크다.
- 이 문서는 법률 자문이 아니다. 공개 전에 걸리는 부분(특히 AI 약관)은 해당 회사에 직접 물어보는 것을 권한다.

---

## 3줄 결론

1. **만들 가치가 있다.** 탐색기 대체 UI, MFT 크기 분석, MFT+USN 즉시 검색, 시각화, 계획→승인→실행 취소가 있는 AI 정리를 모두 갖춘 프로그램은 없다. 가장 가까운 것은 다음 세 가지인데, 모두 AI 정리와 링크 남기고 이동 기능이 없다.
   - Directory Opus·Total Commander에 Everything을 연동한 조합
   - WinDirStat 2.9.x
   - 소규모 Tauri 프로젝트(Glint, Ferret)
2. **GitHub에 공개해도 된다.** 다만 조건이 있다.
   - 공개 Windows API를 직접 구현하고, MIT/Apache 계열 crate만 쓰는 클린룸 구현이면 문제가 없다.
   - GPL 코드(WinDirStat, Explorer++)를 복사하면 프로젝트 전체가 GPL이 된다. MPL 코드(Files 일부 파일)를 복사하면 그 파일만 MPL로 남는다.
   - AI 연결 중 MCP 서버, BYOK, Ollama는 안전하다.
   - Claude Code CLI나 Codex CLI 호출은 "사용자가 직접 설치하고 로그인한 수정되지 않은 바이너리"일 때만 가능하다. 앱이 구독 로그인을 대신 받거나 토큰을 읽는 것은 불가다.
   - TypeSafe는 실제 API 계약서(Master Customer Agreement)가 공개되어 있지 않아 확인할 수 없다.
3. **주의할 점**
   - MFT 읽기와 USN 저널 접근은 관리자 권한이 필요하다. 그래서 관리자 헬퍼는 보호 파일 보기뿐 아니라 고속 스캔에도 필요하고, 이 헬퍼와 IPC가 가장 큰 보안 위험 지점이다.
   - 코드 서명: 한국의 개인은 Azure Artifact Signing을 쓸 수 없다. SignPath Foundation(무료, 조건 있음), OV 인증서, Store MSIX 중에서 골라야 한다.
   - WizTree와 Everything 이름은 사실을 비교하는 문장에만 쓰고, 제품명이나 로고에는 쓰지 않는다.
   - WizTree 바이너리는 역분석하지 않는다(EULA와 저작권법 모두 걸린다).

---

## 질문 1. 비슷한 프로그램이 이미 있는가

### 1.0 비교 기준 열(계획한 기능)

| 기호 | 기능 |
|---|---|
| A | 탐색기형 UI: 탭, 주소창, 미리보기 |
| B | 복사·이동, 휴지통 삭제, 실행 취소 |
| C | 모든 폴더에 크기 열(WizTree처럼) |
| D | NTFS MFT 직접 읽기(고속 스캔) |
| E | MFT+USN 기반 실시간 파일명 검색(Everything처럼) |
| F | 시각화: 트리 목록, 트리맵, 선버스트, 아이시클, 이전 스캔과 비교 |
| G | 관리자 헬퍼: 평소에는 일반 권한, UAC 승인 시에만 상승. 보호 파일 읽기 전용 보기 |
| H | AI 최적화 5종(구조 재편, 이름 변경, 중복·구버전 정리, 용량 축소, D:로 옮기고 링크 남기기), 계획→승인→실행 취소 흐름 |
| I | 정션·심볼릭 링크 만들기(옮기고 링크 남기기의 기반) |
| J | 중복 파일 찾기 |
| K | 실행 파일의 디지털 서명, 게시자, 버전 표시 |

판정은 지원 / 부분 / 미지원 / 확인 불가 네 가지다. "미지원"은 대부분 공식 문서, 기능 목록, 변경 기록에서 찾지 못했다는 뜻이다. 벤더가 "없다"고 명시한 경우가 아니다.

### 1.1 탐색기 대체

| 프로그램 | A | B | C | D | E | F | G | H | I | J | K | 최신 릴리스(유지보수) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **Windows 11 파일 탐색기** | 지원(탭은 2022년 도입) [WE1] | 지원. Ctrl+Z는 Windows 공통 단축키 [WE6] | 미지원 | 미지원 | 부분: Windows Search 인덱스를 씀. MFT/USN을 직접 쓰지 않음 [WE4] | 미지원 | 지원: OS의 UAC 프롬프트 (출처 미확보) | 부분: AI actions(요약, 이미지 편집)와 File Explorer MCP connector(미리보기 단계). 정리 계획이나 실행 취소 흐름은 없음 [WE2] [WE3] [WE4] | 미지원(UI에 없음, mklink 명령만) | 미지원 | 지원: 속성의 디지털 서명·자세히 탭 (출처 미확보) | OS에 포함, 26H2 문서는 2026-09-29 [WE4] |
| **Files** (files-community) | 지원 [FIL1] | 지원: Undo/Redo 액션, 휴지통 복원 [FIL9] | 부분: "Calculate folder sizes" 옵션. 문서가 CPU 부하를 경고함. 결과는 캐시함 [FIL3] | 미지원 | 미지원: Everything 연동 이슈 #5845는 아직 열려 있음. 인덱스 검색 PR #18551은 병합 없이 닫힘 [FIL4] | 미지원 | 지원: 관리자 권한 확인 대화상자 [FIL8] | 부분: 자체 AI는 없음. 서드파티 WindowSill 연동만 있음 [FIL11] | 미지원(.lnk 바로가기만) | 미지원 | 부분: 속성에 서명 목록 [FIL10] | v4.3 정식 2026-10-11, v4.3.1 Preview 2026-10-20. 매우 활발 [FIL2] |
| **OneCommander** | 지원 [OC1] | 지원: 3.71.2에서 Undo 추가 [OC2] | 지원: 수동 계산 후 캐시. V4는 자체 폴더 크기 DB [OC6] [OC4] | 미지원(언급 없음) | 미지원/확인 불가: 공식 사이트에 Everything 언급 0건 [OC2] | 미지원 | 확인 불가 | 미지원 | 부분: Pro에서 Alt-드롭으로 심링크·하드링크. 정션 없음 [OC2] [OC5] | 미지원 | 부분: V4에서 실행 파일 상세 정보 강화 [OC4] | V3 3.108.0.0. 홈페이지는 2026-09-26, releasenotes.txt는 2026-03-06으로 서로 다름(충돌). V4 Beta 0.0.77 [OC1] [OC2] [OC4] |
| **Directory Opus** | 지원 [DO2] | 지원: Undo 목록과 로그. 휴지통 삭제도 되돌릴 수 있음 [DO3] | 지원: Everything이 있으면 그 인덱스로 고속 계산 [DO4] [DO5] | 미지원: 자체 MFT 리더 문서 없음. 속도는 Everything에서 나옴 | 부분: Everything을 설치하면 검색창과 Find에서 사용 [DO5] | 부분: 평가 열의 Graph 표시만. 트리맵 없음 | 지원: Admin 모드 Lister, `@admin` 수정자 [DO6] | 미지원: 공식 AI 없음, 커뮤니티 스크립트만 [DO13] | 지원: 정션, 하드링크, 소프트링크. 옮기고 링크 남기는 단일 명령은 없음 [DO7] | 지원(이름+크기 또는 MD5) [DO8] | 지원/부분: 제품 버전 열. 서명은 스크립트(`FSUtil.GetSignature`)로 [DO9] [DO10] | 13.25 정식 2026-08-21, 13.25.4 베타 2026-09-28 [DO1] |
| **XYplorer** | 지원 [XY3] | 지원: 다단계 Undo/Redo, 세션 간 저장 [XY2] | 지원: 자체 계산과 Folder Size Caching [XY3] | 미지원 | 부분: Everything은 커뮤니티 스크립트로만 [XY5] | 미지원 | 부분: 기능 목록에 "UAC" 항목만 [XY3] | 미지원 | 지원: NTFS 정션·심링크 [XY3] | 지원(파일, 이미지) [XY2] | 확인 불가 | 28.40.0600, 2026-10-13 [XY1] [XY4] |
| **Total Commander** | 지원: 탭, Quick View [TC5] | 부분: 복사·이동·휴지통은 지원. 일반 파일 작업의 실행 취소는 없음(다중 이름 변경만 가능) [TC3] [TC4] | 지원: "Automatic, only with Everything" 옵션 [TC3] | 미지원 | 지원: Find Files가 Everything을 씀(`ev:` 접두사) [TC3] | 미지원 [TC4] | **지원: `tcmadmin.exe` 헬퍼가 UAC로 상승해 복사·삭제를 처리** [TC3] | 미지원 | 지원: 정션, 하드링크, 소프트링크 [TC6] | 지원 [TC4] | 확인 불가 | 11.58 정식 2026-07-01 [TC1] |
| **Explorer++** | 지원 [EPP1] | 부분: 실행 취소는 이름 변경만 [EPP3] | 부분: 동기 계산. 문서가 성능 저하를 경고함 [EPP5] | 미지원 | 미지원 [EPP1] | 미지원 | 부분: 권한 수준 표시, 관리자 cmd 열기 [EPP3] | 미지원 | 미지원 | 미지원 | 확인 불가 | 정식은 1.4.0(2024-01-28). 커밋은 2026-10-18까지 활발 [EPP4] |
| FreeCommander XE | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | 확인 불가 | XE 2026 Build 941, "01.02.2026"(날짜 형식이 모호함) [FC1] |
| Double Commander | (기능은 조사하지 않음) | | | | | | | | | | | 1.2.9, 2026-09-27 [DC1] |

### 1.2 디스크 용량 분석

| 프로그램 | A | B | C | D | E | F | G | H | I | J | K | 최신 릴리스 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **WizTree** | 미지원: Tree View/File View 탭만 있고 탐색기형은 아님 [WT3] | 확인 불가(가이드에 문서 없음) | 지원 [WT1] | **지원: 관리자로 실행할 때만** [WT3] | 미지원: 자매 제품 WizFile 담당 [WF1] | 부분: 트리맵, 트리 목록. 선버스트·아이시클·스캔 비교는 문서 없음 [WT3] | 부분: MFT 스캔은 관리자로 재시작해야 함. `/admin=0` 옵션 [WT3] [WT2] | 미지원 [WT2] | 확인 불가 | 부분: 이름·크기·날짜만 비교, 내용은 비교하지 않음 [WT3] | 확인 불가 | 4.33, 2026-09-16(macOS판 추가) [WT2] |
| **WinDirStat** | 부분: Explorer 연동, 열기·이동·삭제 [WDS1] | 부분: 실행 취소 문서 없음 [WDS1] | 지원 [WDS1] | **지원: 관리자로 실행하면 NTFS 가속 스캔** [WDS1] [WDS3] | 미지원(파일 시스템 변경 감시는 있음) [WDS1] | **부분~지원: 트리맵(Hilbert/Moore), 선버스트, 플레임 그래프(2.8.0~). "아이시클"이라는 이름은 없음. 이전 스캔 비교는 확인 불가** [WDS3] | 부분: "Run Elevated" 메뉴 [WDS1] | 미지원 | 확인 불가 | 지원: 해시 기반 [WDS1] | 확인 불가 | 2.9.3 정식 2026-10-11, 매우 활발 [WDS2] |
| SpaceSniffer | 미지원 | 확인 불가 | 지원(트리맵에 크기) [SS1] | 확인 불가 | 미지원 | 부분(트리맵) [SS1] | 확인 불가 | 미지원 | 확인 불가 | 확인 불가 | 확인 불가 | 2.2.0.27, 날짜 표기 없음. 사이트 저작권 표기가 2000–2016이라 유지보수 상태가 불분명 [SS1] |
| **TreeSize** (Pro / Free) | 부분: "Explorer-like interface", 복사·이동·삭제·일괄 이름 변경 [TS5] | 부분: 실행 취소는 확인 불가 | 지원 [TS5] | 지원: 로컬 NTFS에서 고속 스캔(벤더 설명) [TS5] | 미지원 | 부분: 파이·막대·트리맵. Pro는 저장한 스캔과 비교 가능 [TS5] | 부분: "관리자로 시작" 버튼 [TS6] | 부분: 9.9에 앱 사용법을 답하는 챗봇 "JAMie". 파일 최적화는 아님 [TS1] | 확인 불가 | 지원(Personal/Pro) [TS5] | 확인 불가 | Pro 9.9.1(2026-10-14), Free 4.8.1(2025-11-20) [TS1] [TS2] |

### 1.3 빠른 검색

| 프로그램 | A | B | C | D | E | F | G | H | I | J | K | 최신 릴리스 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **Everything** (voidtools) | 부분: 1.5 베타에 탭, 폴더 사이드바 [EV2] [EV3] | 부분: 1.5 베타에 Undo 시스템 [EV2] | 지원: "Index folder size" 옵션 [EV9] | 지원: NTFS 저수준 읽기 [EV8] | **지원: USN 저널로 인덱스를 최신 유지** [EV8] | 미지원 | **지원: Everything Service로 UAC 프롬프트 없이 인덱싱** [EV8] | 미지원 | 미지원 | 지원(1.5 "Find duplicates") [EV3] | 확인 불가 | 정식 1.4.1.1032(2026-01-23), 베타 1.5.0.1424b(2026-10-15). 1.5 페이지는 1423b로 표기(충돌) [EV1] [EV2] [EV3] |
| UltraSearch | 확인 불가 | 부분: 일괄 이동·삭제·이름 변경은 Pro [US2] | 확인 불가 | 지원: 관리자 권한이 없으면 MFT에 접근하지 못해 느려짐 [US3] | 확인 불가(USN 명시 없음) | 미지원 | 부분 [US3] | 미지원 | 확인 불가 | 확인 불가 | 확인 불가 | 4.9.4, 2026-09-22 [US1] |
| SwiftSearch | 미지원 | 확인 불가 | 지원("Full directory sizes") [SW1] | 지원: NTFS 테이블 직접 읽기, 관리자 필요 [SW1] | 부분: 검색할 때마다 다시 읽음. USN은 확인 불가 [SW1] | 미지원 | 관리자 필요 [SW1] | 미지원 | 미지원 | 미지원 | 미지원 | SourceForge 최종 갱신 2019-07-06(비활성) [SW1] |
| WizFile | 미지원 | 확인 불가 | 확인 불가 | 지원 [WF1] | 지원: 실시간 결과 갱신. USN은 명시 없음 [WF1] | 미지원 | 확인 불가 | 미지원 | 미지원 | 미지원 | 미지원 | 확인 불가 |

### 1.4 AI 파일 정리 도구와 신생 Rust/Tauri 탐색기(2025–2026)

| 도구 | 하는 일 | AI 연결 | 계획→미리보기→승인→실행 취소 | MFT/USN | 플랫폼·가격 | 라이선스 | 최신 |
|---|---|---|---|---|---|---|---|
| **AI File Sorter** (hyperfield) [AFS1] | 분류와 하위 폴더 정리, 이미지·문서·미디어 이름 변경 | 내장 llama.cpp(GGUF). OpenAI·Gemini 키 직접 입력. OpenAI 호환 엔드포인트로 Ollama 사용 | 검토 대화상자, 드라이런, "Undo last run"(최선 노력) | 없음 | Win/macOS/Linux, MS Store 무료 [AFS3] | AGPL-3.0 | v1.9.2, 2026-08-14 [AFS2] |
| Local-File-Organizer [LFO1] | 내용 기반 재편과 이름 변경 | Nexa SDK 로컬 모델 | 드라이런만, 실행 취소 없음 | 없음 | Python, 크로스플랫폼 | MIT(README에는 MIT+Apache로 표기, 충돌) | v0.0.2(2024-09), 사실상 중단 |
| LlamaFS [LFS1] | 이름 변경과 분류, 감시 모드 | Groq(클라우드) 또는 Ollama | 검토 UI 있음, 실행 취소 없음 | 없음 | Electron | MIT | 마지막 커밋 2025-08-08 |
| FileWizardAI [FWA1] | 정리, 이름 변경, 검색 | Ollama, Groq, OpenAI | 문서 없음 | 없음 | Win/Linux | MIT | 커밋 4개, 정체 |
| Sortio (상용) [SOR1] | 프롬프트로 이름 변경과 이동, 감시 폴더 | 클라우드, Ollama, 직접 키(OpenAI, Anthropic, OpenRouter, LM Studio) | **계획 미리보기, 백업, 일괄 되돌리기, 이동 로그** | 없음 | Win/macOS/Linux. 무료 등급, Pro $14.99/월 또는 $99/년 | 상용 | 확인 불가 |
| RenameClick (상용) [RC1] | 이름 변경, 분류 | 내장 로컬 모델, Ollama, LM Studio, 직접 키 | 적용 전 검토, 세션 한정 실행 취소 | 없음 | Mac/Win. 무료 100건/월, $36 평생 | 상용 | v3.1.0 |
| Renamer.ai (상용) [RN1] | 내용 기반 이름 변경, 폴더 템플릿 | 자체 클라우드, 직접 키, 로컬 | 미리보기와 Revert | 없음 | Win/Mac, 무료 25건/월 | 상용 | 확인 불가 |
| NameQuick (상용) [NQ1] | 이름 변경, 규칙 기반 이동 | 관리형, 직접 키, Ollama, LM Studio | "Review first", Undo 기록 | 없음 | Mac/Win(x64) | 상용 | 확인 불가 |
| **ai-disk-cleaner** (vudsen) [ADC] | LLM이 정리할 파일 표시, **다른 드라이브로 옮기고 심링크 남기기** | 문서 없음 | 문서 없음 | gdu 사용(MFT 아님) | Windows | MIT | 릴리스 없음 |
| AI File Explorer (Jesusfloww) [AFE] | WinUI 파일 관리자 안의 AI 정리 제안 | llama.cpp(Qwen3-8B) 로컬만 | 제안 검토·편집, 영구 Undo/Redo | 없음 | Windows | MIT | 커밋 2개, 별 0개 |
| **Spacedrive v2** [SD1] | 탐색기, 실시간 인덱스, 검증된 중복 정리, 스냅숏 | **내장 AI 없음. CLI, SDK, MCP 서버를 외부 에이전트에 노출**(실행 전 미리보기) | 미리보기 | MFT 아님(감시자와 SQLite FTS5) | 크로스플랫폼. Windows는 알파 품질 | **Apache-2.0(2026-03-24 이전 릴리스는 AGPL-3.0)** | v2.0.0-alpha.2, 2.0 베타는 2026-11-01 예정 [SD2] |
| Xplorer [XP1] | Tauri 2 탐색기 | 사이트는 Ollama 기반 AI 채팅·의미 검색을 주장 [XP2] | 확인 불가 | 확인 불가 | 크로스플랫폼 | AGPL-3.0 | v1.0.0-alpha.1 |
| Sigma File Manager [SFM] | Tauri 탐색기, 전역 검색 | 없음 | 확인 불가 | 확인 불가 | 크로스플랫폼 | GPL-3.0-or-later | v2.2.0 |
| **Glint** (shadijhade) [GL1] | Rust/Tauri 2 탐색기, 폴더 크기 열, 트리맵, 중복, 일괄 이름 변경 | 없음 | 확인 불가 | MFT/USN은 PR #3에서 아직 제안 단계 [GL2] | Windows | MIT | 별 5개, 릴리스 없음 |
| **Ferret** (meltingice1337) [FE1] | Tauri 탐색기, 백그라운드 폴더 크기, 복사·이동·삭제·압축의 Undo/Redo | 없음 | 실행 취소 있음 | **MFT 인덱싱 + USN 추적(선택형 서비스)** | Windows | 표기 없음 | 별 1개 |
| Lertaro [LT1] | C# 검색 런처, 트리맵, 정리 동작 | 없음 | 확인 불가 | MFT + USN, SYSTEM 서비스로 설치 | Windows | MIT | 별 약 1.2k |
| Omni-search [OS1] | Tauri/Rust/C++ MFT/USN 검색, 해시 중복, 휴지통 삭제 | 없음 | 확인 불가 | MFT + USN | Windows | MIT | 별 약 600 |
| Windows File Explorer MCP connector [WE3] | MCP 도구: 파일 정보, 이동·이름 변경, 생성, zip, 검색 | MCP(에이전트가 연결) | **삭제 도구 없음, 실행 취소 없음**, 사용자 허가 필요 | Windows Search | Windows 11(미리보기 빌드) | MS | 2025-11-18 문서 |
| 공식 filesystem MCP 서버 [MCPFS] | 허용한 디렉터리에서 읽기, 쓰기, 이동 | MCP | `edit_file`에 dryRun. 삭제 도구 없음 | 없음 | Node | MIT | 확인 불가 |
| mcp-everything-search [MCPEV] | Everything SDK를 MCP로 감싼 래퍼 | MCP | 해당 없음 | Everything 경유 | Windows | MIT | v0.3.0 |
| Claude Cowork (Claude Desktop) [COW] | 폴더 접근을 허락하면 내용 기준으로 파일 정리 | Anthropic 클라우드 | 확인 불가 | 없음 | Win/macOS, 유료 플랜 | 상용 | 확인 불가 |
| CCleaner 7.12 [CC1] | ChatGPT·Claude·Cursor 등 AI 앱이 남긴 찌꺼기를 지우는 정리 항목. AI 비서는 아님 | 없음 | 해당 없음 | 없음 | Windows | 상용 | 7.12.1526, 2026-09-29 |

### 1.5 이것들을 모두 합친 프로그램이 있는가

**없다.** 탐색기 대체 UI, MFT 크기 분석, MFT+USN 즉시 검색, 다양한 시각화, 관리자 헬퍼, 계획→승인→실행 취소형 AI 최적화를 한 프로그램에 담은 사례는 찾지 못했다.

가장 가까운 것과 각각 빠진 기능:

| 후보 | 갖춘 기능 | 빠진 기능 |
|---|---|---|
| Directory Opus 또는 Total Commander + Everything | A, B, C(Everything 경유), E(Everything 경유), G, I, J | 자체 MFT, 시각화, AI, 옮기고 링크 남기기 단일 동작. 별도 설치한 Everything에 의존 [DO5] [TC3] |
| WinDirStat 2.9.x | C, D(관리자 시), F(트리맵·선버스트·플레임), J(해시) | 탐색기형 UI, USN 실시간 검색, AI, 이전 스캔 비교 확인 불가 [WDS1] [WDS3] |
| Ferret / Glint (Tauri, 소규모) | 탐색기 + 폴더 크기 + MFT/USN(Ferret) + 실행 취소(Ferret), 트리맵(Glint) | AI 없음. 별 1~5개 수준이고 기능 주장은 README뿐 [FE1] [GL1] |
| Spacedrive v2 | 탐색기, 인덱스, 중복 정리, MCP 노출 | MFT 없음, 내장 AI 없음, Windows는 알파 [SD1] |
| AI File Sorter / Sortio | AI 이름 변경·분류 + 미리보기 + 되돌리기 | 탐색기, MFT, 크기 분석, 시각화 없음 [AFS1] [SOR1] |

시장 공백은 세 가지다. 근거는 위 표들이다.

- MFT 인덱스 위에서 도는 AI 정리 계획을 실행 취소까지 지원하는 것
- "D:로 옮기고 정션 남기기"를 승인과 실행 취소가 되는 동작으로 만드는 것(ai-disk-cleaner만 비슷함)
- 보호 파일을 읽기 전용으로 보는 관리자 헬퍼

이미 있는 아키텍처 선례도 있다. Everything은 서비스로 UAC 없이 인덱싱하고 [EV8], Total Commander는 `tcmadmin.exe` 헬퍼로 필요할 때만 상승한다 [TC3]. 계획한 "일반 권한 UI + 승인 시 헬퍼" 구조는 검증된 패턴이다.

---

## 질문 2. 유료인가 무료인가

| 프로그램 | 가격 | 개인 무료? | 상업적 이용 조건 | 오픈소스? | 라이선스 |
|---|---|---|---|---|---|
| Windows 파일 탐색기 | OS에 포함 | 해당 없음 | Windows 라이선스 | 아니오 | 독점 |
| Files | 무료 설치판 있음. 문서는 Store 구매를 권함(가격은 확인 불가) [FIL12] | 예 | 제한 없음(라이선스 조건만 지키면 됨) | 예 | **MIT + MPL-2.0 혼합.** 루트에 LICENSE-MIT와 LICENSE-MPL이 있고, MPL은 2026-06-30 PR #18657로 추가됨. 파일마다 헤더가 다름 [FIL5] [FIL6] [FIL7] |
| OneCommander | Pro $30(1인 평생). 5인 이상 회사는 견적 [OC3] | 예(비상업 가정용, 일부 설정 잠김) | 업무·프리랜서 이용은 Pro 필요 [OC3] | 아니오 | 독점 프리미엄 |
| Directory Opus | 확인 불가(공식 페이지 가격이 JS로 렌더링됨. A$89는 2차 기사). 갱신 A$25/년 [DO11] [DO12] | 아니오(30~60일 체험) | 유료 라이선스 | 아니오 | 독점 |
| XYplorer | Standard $34.95, Lifetime $69.95 [XY2] | 아니오(30일 체험) | 유료 | 아니오 | 독점 셰어웨어 |
| Total Commander | EUR 42 + VAT [TC2] | 아니오(30일 후 구매 또는 삭제) | 유료(개인과 상업 구분 없음) | 아니오 | 독점 셰어웨어 |
| Explorer++ | 무료 | 예 | 제한 없음(GPL 조건) | 예 | **GPL-3.0-only** [EPP2] |
| FreeCommander XE | 64비트판은 유료(가격 페이지 404) | 32비트판은 개인 무료 [FC2] | 상업 사용자는 30일 체험 후 유료 [FC2] | 아니오 | 독점 |
| Double Commander | 무료 | 예 | 제한 없음 | 예 | GPL-2.0(only인지 or-later인지는 확인 불가) [DC1] |
| **WizTree** | 개인 무료. 기업은 사이트 규모별 $100~$1,800 영구 라이선스. 번들 배포 라이선스는 $1,200/년 [WT4] [WT6] | **예(개인 용도만)** [WT5] | "commercial environment"에서 쓰려면 유료 코드 필요 [WT5] | 아니오 | 독점 EULA. 역분석·재배포 금지 [WT5] |
| **WinDirStat** | 무료 | 예 | 제한 없음(GPL 조건) | 예 | **GPL v3 or later(2.9.2부터). 2.9.0까지는 GPL v2**("v2보다 높은 버전으로 올릴 수 없다"는 문구 있음) [WDS1] [WDS3] [WDS4] |
| SpaceSniffer | 무료(기부) [SS1] | 예 | 확인 불가(EULA 없음) | 아니오 | 프리웨어 |
| TreeSize | Free €0. Personal €20.40/년. Pro €41/사용자/년부터 [TS3] [TS4] | Free는 비상업 개인만, 서버 제외 [TS3] | Pro 필요 | 아니오 | 독점 |
| **Everything** | 무료 | 예 | **허용**(MIT식 라이선스. 번들 재배포도 허용한다는 개발자 답변 있음) [EV4] [EV5] | **아니오**(소스 비공개, 바이너리만 MIT식) | MIT식(Copyright 2018 David Carpenter) + PCRE BSD 고지 [EV4] |
| Everything SDK | 무료 | 예 | 개발자 포럼 답변(2018)에 "The SDK uses the MIT license" [EV7]. Everything 클라이언트가 실행 중이어야 함 [EV10] | SDK 소스 공개 | MIT(포럼 답변 기준, 정식 SDK 라이선스 파일은 확인 못 함) |
| UltraSearch | Free / Pro €24/사용자/년부터 [US2] | Free는 비상업 개인 | Pro 필요 | 아니오 | 독점 |
| SwiftSearch | 무료 | 예 | **상업 이용 불가(CC BY-NC 2.0)** [SW1] | 소스 공개(OSI 라이선스 아님) | CC BY-NC 2.0 |
| AI File Sorter | 무료 [AFS3] | 예 | AGPL 조건 | 예 | AGPL-3.0 [AFS1] |
| Spacedrive | 무료 | 예 | 제한 없음 | 예 | Apache-2.0(2026-03-24부터) [SD1] |
| Sortio / RenameClick / Renamer.ai / NameQuick | 무료 등급 + 구독 또는 평생 라이선스(1.4절 참고) | 제한된 무료 | 유료 | 아니오 | 상용 |

---

## 질문 3. 직접 구현해서 GitHub에 공개해도 되는가

### 3.1 공개 Windows API(FSCTL_ENUM_USN_DATA, USN 저널, $MFT 읽기)로 같은 기능을 구현해 공개하기: **가능**

근거:
- 공개 문서로 제공되는 API다.
  - `FSCTL_ENUM_USN_DATA`는 "MFT 레코드를 얻기 위해 USN 데이터를 열거"하는 제어 코드로, Windows XP부터 지원된다 [MS1].
  - `FSCTL_GET_NTFS_FILE_RECORD`는 파일 레코드를 돌려준다 [MS3].
  - USN 변경 저널은 백업·인덱싱 프로그램이 볼륨 전체를 다시 훑지 않도록 NTFS가 제공하는 기능이라고 설명되어 있다 [MS2].
  - 이 문서들에 "이 API로 경쟁 제품을 만들면 안 된다" 같은 이용 제한은 없다 [MS1] [MS2] [MS3].
- MFT 자체에 대한 공식 개념 문서도 있다 [MS6].
- 이 기능을 쓰는 오픈소스 Rust crate가 이미 MIT/Apache로 배포되고 있다(`usn-journal-rs`, `ntfs`, `mft`, `ntfs-reader`. 3.4절 참고).

조건과 주의:
- **관리자 권한이 필요하다.** 볼륨 핸들(`\\.\C:`)을 직접 열려면 CreateFile 문서상 호출자가 관리자여야 한다("The caller must have administrative privileges") [MS4]. USN 작업도 같은 형식의 볼륨 핸들을 쓴다 [MS5]. 그래서 계획한 "일반 권한 UI"만으로는 MFT/USN 스캔을 할 수 없다.
  - 대안 1: 스캔도 UAC 승인 헬퍼가 맡는다.
  - 대안 2: Everything처럼 서비스를 설치한다 [EV8].
  - 대안 3: 관리자 권한이 없을 때는 일반 디렉터리 열거로 떨어진다. WizTree도 비관리자 모드가 있다 [WT3].
- **문서 텍스트 복사는 피한다.** Microsoft Learn 이용약관은 문서를 개인적·비상업적으로만 쓰도록 하고, 수정과 재배포를 제한한다 [MS7]. 반면 원본 저장소 MicrosoftDocs/win32에는 CC-BY-4.0과 MIT 라이선스가 걸려 있다 [MS8](어느 쪽이 코드 샘플에 해당하는지는 확인 불가). 샘플 코드를 붙여 넣기보다 자기 말로 쓰고 링크만 다는 것이 안전하다.
- 원시 디스크 읽기는 실수하면 데이터 손상 위험이 있다고 문서가 경고한다 [MS4]. 헬퍼에서 쓰기 접근은 처음부터 열지 않는다.

### 3.2 WizTree, Everything의 이름이나 UI 배치를 참고하고 비교 대상으로 언급하기: **조건부 가능**

**WizTree EULA(공식 온라인판)** [WT5]
- 사용자의 로컬 license.txt와 같은 취지다. "decompile, disassemble or reverse engineer the Software"를 금지하고, 복제·배포·재판매도 금지한다.
- 개인 용도만 무료다.
- 상표 조항은 없다. 확인한 것은 저작권 표기(© 2026)뿐이다.
- 따라서 WizTree 바이너리를 분석하지 않고 공개 API로 같은 기능을 처음부터 만드는 것은 EULA와 무관하다. EULA는 WizTree 소프트웨어 사용자를 구속하는 계약이고, 기능 아이디어를 막지 않는다.
- 주의: WizTree를 설치해 쓰면서 동작을 관찰하는 것은 정상 사용이다. 디버거나 디스어셈블러를 붙이는 순간 EULA 위반이다.

**Everything 라이선스** [EV4]
- MIT식 허가 문구다(Copyright 2018 David Carpenter).
- 역분석 금지 조항이 없고, 사용·복제·수정·배포·판매를 허용한다. 개발자는 번들 재배포도 괜찮다고 답했다 [EV5].
- 단, 소스는 공개되어 있지 않다.
- Everything SDK를 "선택적 백엔드"로 붙이면 MIT 고지를 포함해야 한다 [EV7]. SDK는 Everything 프로그램이 실행 중이어야 동작한다 [EV10].

**이름과 상표**
- voidtools와 WizTree 공식 사이트에서 상표 고지는 찾지 못했다. 한국 특허청(KIPRIS) 등록 여부는 조사하지 않았다(확인 불가).
- 일반적으로 안전한 범위:
  - 사실에 근거한 비교 문장. 예: "WizTree처럼 MFT를 직접 읽어 빠르게 스캔한다", "Everything과 같은 방식(USN 저널)".
  - 벤치마크는 조건을 명시하고 재현 가능하게 공개한다.
- 피해야 할 것:
  - 제품명, 저장소명, 아이콘, 로고에 상대 이름 넣기. 예: "WizTree2", "Everything for X".
  - 공식 파트너나 호환 인증처럼 보이는 표현.
- 참고 사례: Anthropic도 자사 이름을 제품명이나 로고에 쓰는 것을 금지하고, "runs Claude Code"처럼 사실 진술만 허용한다 [AN1]. 업계의 일반적인 기준선으로 볼 수 있다.

**UI 배치**
- 한국 저작권법은 프로그램의 "해법"(지시·명령을 조합하는 방법)과 규약, 프로그래밍 언어를 보호 대상에서 뺀다(제101조의2) [KR4]. 기능, 알고리즘, 일반적인 배치(트리 + 크기 열 + 트리맵)는 아이디어 영역이다.
- 다만 아이콘, 그림, 화면 디자인의 구체적 표현을 그대로 복제하면 저작권이나 부정경쟁 문제가 생길 수 있다. 자체 아이콘과 자체 색 체계를 쓴다.

### 3.3 참고 오픈소스 코드를 일부 복사하면 프로젝트 라이선스에 생기는 영향

| 원본 | 실제 라이선스(확인) | 복사하면 | 판정 |
|---|---|---|---|
| **Files** | 혼합: 기존 파일 대부분은 `Licensed under the MIT License` 헤더(예: `IAction.cs`), 2026-06-30 이후 일부는 `SPDX-License-Identifier: MPL-2.0` 헤더(예: `VolumeInfo.cs`) [FIL5] [FIL6] [FIL7] | MIT 파일: 저작권 고지와 MIT 전문을 유지하면 내 프로젝트 라이선스는 영향 없음. MPL-2.0 파일: **그 파일(과 수정본)만** MPL-2.0으로 남고 소스를 제공해야 함. 나머지 코드는 MIT/Apache로 둘 수 있음(파일 단위 copyleft) [FIL5] [GNU1]. C#를 Rust로 번역한 것도 2차적 저작물로 봐야 안전함 | 조건부 가능 |
| **WinDirStat** | **2.9.2부터 GPL v3 or later** [WDS3] [WDS1]. **2.9.0까지는 GPL v2**, README에 "v2보다 높은 버전으로 올릴 수 없다"는 문구 [WDS4]. 로고는 CC BY 3.0 | 코드 일부라도 복사하면 **프로젝트 전체를 GPL-3.0-or-later로 배포**해야 함. 구버전(GPLv2만) 코드는 Apache-2.0과 호환되지 않음(FSF 기준) [GNU1]. 그래서 Apache-2.0 전용 crate(`rmcp` 등)와 같이 쓰면 충돌함. MIT/Apache 공개가 목표면 **복사하지 말고**, 트리맵 같은 알고리즘은 논문이나 일반 설명을 보고 직접 구현할 것(알고리즘은 저작권 보호 대상 아님 [KR4]) | 조건부 가능(GPL을 받아들일 때만) |
| Explorer++ | GPL-3.0-only [EPP2] | 프로젝트 전체가 GPL-3.0 | 조건부 가능 |
| AI File Sorter | AGPL-3.0 [AFS1] | 프로젝트 전체가 AGPL-3.0 | 조건부 가능 |
| Spacedrive | Apache-2.0(2026-03-24부터), 그 전 릴리스는 AGPL-3.0 [SD1] | 새 버전 코드는 NOTICE와 고지만 지키면 됨. 옛 버전 코드는 AGPL | 가능 / 조건부 |
| 공식 filesystem MCP 서버 | MIT [MCPFS] | 고지 유지 | 가능 |
| WizTree | 독점 [WT5] | 소스가 없음. 복사·역분석 모두 금지 | **불가** |
| Everything | 바이너리만 MIT식 [EV4] | 소스가 없어 복사할 대상이 없음. SDK 코드는 MIT(포럼 답변 기준) [EV7] | 가능(SDK) |

### 3.4 후보 Rust crate의 라이선스와 서로의 호환성 (crates.io API로 2026-10-21 조회)

| crate | 용도 | 최신 버전 | 라이선스 | 최근 갱신 |
|---|---|---|---|---|
| `ntfs` [CR1] | 원시 NTFS 볼륨 파싱(Colin Finck) | 0.4.0 | MIT OR Apache-2.0 | 2023-06-13(갱신이 뜸함) |
| `mft` [CR2] | $MFT 파일 파싱(포렌식 계열) | 0.7.0 | MIT/Apache-2.0 | 2025-12-23 |
| `ntfs-reader` [CR8] | MFT 읽기와 USN | 0.6.0 | MIT OR Apache-2.0 | 2026-10-01 |
| `usn-journal-rs` [CR3] | USN 저널 열거·추적 | 0.5.0 | MIT | 2026-10-13 |
| `usn-parser` [CR11] | USN 레코드 파서 | 0.1.5 | MIT | 2026-10-13 |
| `windows` / `windows-sys` [CR4] [CR5] | Win32 바인딩(Microsoft) | 0.62.2 / 0.61.2 | MIT OR Apache-2.0 | 2025-10-06 |
| `tauri` [CR6] | 앱 프레임워크 | 2.12.4 | Apache-2.0 OR MIT | 2026-10-14 |
| `tauri-plugin-shell` [CR12] | 외부 CLI 실행 | 2.5.0 | Apache-2.0 OR MIT | 2026-10-14 |
| `rmcp` [CR7] | 공식 MCP Rust SDK | 3.6.0 | **Apache-2.0만** | 2026-10-20 |
| `trash` [CR9] | 휴지통 삭제 | 5.2.9 | MIT | 2026-09-13 |
| `junction` [CR10] | 정션 만들기 | 2.1.0 | MIT | 2026-09-24 |

- MCP 명세 저장소는 MIT에서 Apache-2.0으로 전환하는 중이다. 새 기여는 Apache-2.0, 문서는 CC-BY-4.0이다 [MCPL].
- 호환성: 모두 허용형 라이선스다. MIT 전용 crate와 Apache-2.0 전용 crate(`rmcp`)를 같이 써도 충돌하지 않는다. 배포할 때 각 라이선스 고지를 모아 넣으면 된다(`cargo about`이나 `cargo-deny` 같은 도구를 쓰면 편하다. 도구 자체는 따로 검증하지 않음).
- GPLv3 프로젝트에서도 이 crate들을 모두 쓸 수 있다. FSF는 Apache-2.0이 GPLv3과 호환되고 GPLv2와는 호환되지 않는다고 본다 [GNU1].

**프로젝트 라이선스 추천: `MIT OR Apache-2.0` 이중 라이선스**
1. Tauri, windows-rs, ntfs, mft 같은 핵심 의존성이 모두 이 조합이다. Rust 생태계의 사실상 표준이라 기여자와 재사용자 모두 편하다 [CR1] [CR4] [CR6].
2. Apache-2.0 쪽은 특허 허락과 특허 보복 조항을 준다. FSF도 큰 프로그램에는 Apache 2.0을 권한다 [GNU1].
3. SignPath Foundation의 무료 서명 조건(OSI 승인 라이선스, 상업용 이중 라이선스 금지)을 만족한다 [SP2].
4. 이후에 GPL 코드(WinDirStat 등)를 꼭 가져와야 한다면 그때 GPL-3.0-or-later로 바꿀 수 있다. 반대 방향(GPL에서 MIT로)은 모든 기여자의 동의가 필요해 사실상 어렵다.

GPL을 고르는 경우: WinDirStat의 코드나 알고리즘 구현을 직접 재사용하고 싶고, 다른 사람이 닫힌 포크를 만드는 것을 막고 싶다면 GPL-3.0-or-later가 합리적이다. 대신 독점 플러그인이나 SDK를 섞는 데 제약이 생긴다.

### 3.5 AI 연결 방식별 공개 가능성 (2026-10-21 약관 원문 기준)

**먼저 확인한 핵심 사실: Anthropic 현행 "Legal and compliance" 페이지** [AN1]
- 2026년 2월 보도는 "Free/Pro/Max OAuth는 Claude Code와 Claude.ai 전용이고, Agent SDK를 포함한 다른 제품에서 쓰면 소비자 약관 위반"이라고 전했다(2차) [SEC1] [SEC2].
- 현재 원문은 그 문장을 다음과 같이 바꿨다. 2026년 10월 기사가 말한 "재작성"이 원문으로 확인된다.
  - OAuth는 Free·Pro·Max·Team·Enterprise 구매자 전용이고, Claude Code와 "other native Anthropic applications"의 일반적인 사용을 위한 것이다.
  - 제품을 만드는 개발자는(Agent SDK 포함) Console이나 지원 클라우드의 API 키를 쓴다.
  - 서드파티가 Claude.ai 로그인을 자기 앱에 제공하거나, 사용자 대신 구독 자격증명으로 요청을 중계하는 것은 허용하지 않는다.
  - Claude.ai 자격증명이나 세션 토큰을 수집·저장·중계해서는 안 된다.
  - 반면 "수정되지 않은 Claude Code 바이너리에 최종 사용자가 자기 구독으로 로그인하는 것"은 막지 않는다고 명시한다.
- 2월 원문 자체는 아카이브로 직접 확인하지 못했다(4장 참고).

| 방식 | 판정 | 근거와 조건 |
|---|---|---|
| **(a) 앱이 MCP 서버를 열고, 사용자의 Claude 앱(구독)이 연결** | **가능** | Claude Desktop은 로컬 MCP 서버를 정식으로 지원한다(설정 파일 또는 확장 설치) [AN6]. 연결 주체는 사용자 본인의 Anthropic 앱이다. file-atlas는 자격증명을 다루지 않으므로 [AN1]의 "Claude.ai 로그인 제공·중계 금지"에 해당하지 않는다. 소비자 약관은 서드파티 통합과 함께 쓸 수 있다고 하고, 그 책임을 사용자에게 둔다(7조) [AN2]. 조건: MCP 도구는 "계획 제안" 위주로 만들고 실제 실행은 앱 UI에서 사용자가 승인하게 한다. 원격 바인딩 없이 stdio나 localhost만 쓴다. 파일 내용 속 프롬프트 주입에 대비한다. SDK `rmcp`는 Apache-2.0 [CR7] |
| **(b) 사용자가 설치한 Claude Code CLI 호출** | **조건부 가능** | Claude Code는 독점 소프트웨어다("All rights reserved", Commercial Terms 적용) [AN7]. 그래서 **번들·재배포는 불가**하고, 사용자가 직접 설치한 바이너리를 호출만 해야 한다. 최종 사용자가 수정되지 않은 바이너리에 자기 구독으로 로그인하는 것은 허용 [AN1]. 앱이 `~/.claude` 자격증명을 읽거나 복사하면 안 됨 [AN1]. **회색 지대:** [AN1]은 "제품 안에서 Claude Code를 사전 설치하거나 실행"하는 경우 Commercial Terms 동의와 조건(바이너리 무수정, 사용량 중계 금지, 사용자 본인 인증)을 요구한다. 오픈소스 데스크톱 앱이 사용자의 CLI를 호출하는 경우가 여기에 해당하는지는 원문만으로 판단할 수 없다(확인 불가, Anthropic 영업팀 문의 권장). **기술적 위험:** 공식 문서는 스크립트 호출에 `--bare` 모드를 권장한다. bare 모드는 구독 로그인을 쓰지 않고 `ANTHROPIC_API_KEY`가 필요하며, 앞으로 `-p`의 기본값이 될 예정이다 [AN5]. 그렇게 되면 CLI 호출은 사실상 BYOK가 된다. 그리고 Pro/Max 사용량 한도는 "ordinary, individual usage"를 전제로 한다 [AN1] |
| **(b') 사용자가 설치한 Codex CLI 호출** | **조건부 가능** | Codex CLI는 Apache-2.0 오픈소스다 [OA7]. 인증은 ChatGPT 로그인(구독) 또는 API 키 [OA1]. 공식 문서는 프로그램에서 호출할 때 API 키 인증을 권하고("Use API key authentication for programmatic Codex CLI workflows"), 신뢰할 수 없는 공개 환경에 Codex 실행을 노출하지 말라고 한다 [OA1]. OpenAI 약관은 계정 자격증명 공유와 타인에게 계정 제공을 금지한다 [OA6]. 조건: 사용자 본인 PC에서, 본인이 로그인한 CLI를, 본인 작업에만 쓴다. `auth.json`은 읽지 않는다 |
| **(b'') "Sign in with ChatGPT" 직접 구현** | **확인 불가(조건부 가능성 있음)** | 단서("파트너 앱 전용")와 원문이 다르다. 개발자 문서는 "commercial partners"는 제한 시범이고, 플랜 사용은 "all open-source partners and selected private clients"에 제공된다고 쓴다 [OA2]. 오픈소스 클라이언트는 `dynamic_agent_client`로 동적 등록하며 비밀키가 필요 없다 [OA3]. 오픈소스 프로젝트가 자동으로 "파트너"가 되는지, 승인 절차가 있는지는 "Preview limitations"와 "Request a client ID" 페이지를 읽지 못해 확인 불가. Anthropic 쪽은 같은 방식이 **불가**하다(Claude.ai 로그인 제공 금지) [AN1] |
| **(c) 사용자 본인 API 키 입력(BYOK)** | **가능** | Anthropic: 제품을 만드는 개발자는 API 키 인증을 쓰라고 명시 [AN1]. 고객이 자기 키를 개발 환경에 설정하는 것도 키 소유자에게 과금되고 재판매·중계하지 않는 한 허용 [AN1]. 소비자 약관은 API 키 공유를 금지한다 [AN2]. 따라서 각 사용자가 자기 키를 넣는 구조여야 한다. OpenAI: Platform API 키를 쓰라는 안내 [OA1]. 구현 조건: 키는 Windows 자격 증명 관리자 등에 저장하고, 저장소·로그·크래시 리포트에 남기지 않는다. 앱 개발자가 자기 키를 내장해 배포하면 중계·재판매에 해당할 수 있으니 하지 않는다 |
| **(c') Ollama(로컬)** | **가능** | 외부 약관이 개입하지 않는다(Ollama 라이선스는 이번에 따로 조사하지 않음) |
| **(d) TypeSafe Jev API** | **확인 불가 → 조건부(BYOK 선택 기능)로만 권장** | 홈페이지는 Jev를 "early access"로 표시한다 [TSA1]. 공개된 Terms of Use는 웹사이트에만 적용되고, 제품·서비스는 별도 계약이 우선한다고 쓴다 [TSA2]. AUP(최종 수정 2026-09-23)는 나머지 조건을 Master Customer Agreement로 넘기는데 [TSA3], **이 MCA는 공개 웹에서 찾지 못했다.** 문서 색인에는 "Legal" 페이지가 있지만 내용을 확인하지 못했다 [TSA6]. API 키는 콘솔에서 발급 [TSA5]. 그래서 **재배포, 래퍼 제작, 결과·벤치마크 공개 조건은 모두 확인 불가**다. AUP에서 확인된 것: 안전 필터 우회 금지, 서비스 소스나 데이터 추출 금지, 출력물이 AI 생성이라는 점을 속이면 안 됨, 위반하면 접근 정지 [TSA3]. 권장: 사용자 본인 키로만 동작하는 선택형 어댑터로 만든다. 저장소에 키, 프롬프트 템플릿 외 응답 덤프, 벤치마크 수치를 올리기 전에 TypeSafe에 MCA를 요청해 확인한다 |

### 3.6 관리자 헬퍼와 보호 파일 보기를 공개할 때 주의할 점

1. **헬퍼의 권한 범위**
   - `FILE_FLAG_BACKUP_SEMANTICS`와 `SE_BACKUP_NAME` 권한을 같이 쓰면 시스템이 파일 보안 검사를 건너뛴다 [MS4]. 아주 강한 권한이다.
   - 헬퍼는 다음만 허용한다.
     - 읽기 전용(`GENERIC_READ`)
     - 허용된 명령 몇 가지(스캔, 메타데이터, 미리보기 바이트 범위)
   - 다음은 막는다.
     - 경로를 정규화하지 않은 요청
     - 임의 실행("관리자로 이 명령 실행")
   - 작업이 끝나면 헬퍼는 바로 종료한다.
2. **IPC 보안**
   - 이름 있는 파이프에 보안 기술자를 주지 않으면 기본 DACL이 Everyone과 익명 계정에도 읽기 권한을 준다 [MS9].
   - 명시적 DACL(현재 사용자 로그온 SID)을 걸어 다른 세션과 원격 접근을 막는다 [MS9].
   - 클라이언트 프로세스를 검증하고, 메시지 스키마를 엄격하게 검사한다.
   - 이 부분은 반드시 테스트하고 위협 모델 문서를 공개한다.
3. **남용 우려**
   - "보호 파일 열람" 기능은 자격증명 저장소 같은 민감 파일 읽기와 겹친다.
   - SignPath Foundation은 보안 취약점 공격·스캐닝 "hacking tools"를 서명 대상에서 뺀다 [SP2].
   - 기능 설명을 "읽기 전용 진단 보기"로 분명히 하고, 위험 경로는 기본으로 차단하거나 경고한다. 해석 여지가 있으니 SignPath에 신청할 때 미리 설명한다(확인 불가).
4. **백신 오탐**
   - 서명 없는 실행 파일이 UAC 상승과 원시 디스크 읽기를 하면 오탐 가능성이 높다(경험칙, 공식 출처 없음).
   - 오탐이 나면 Microsoft Security Intelligence 제출 사이트로 파일을 제출한다 [MS11] [MS12].
5. **코드 서명 선택지와 비용** [MS10]

| 선택지 | 비용 | 한국 개인 이용 | SmartScreen | 비고 |
|---|---|---|---|---|
| Microsoft Store(MSIX) | 무료(Store가 다시 서명) | 가능(전 세계) | 경고 없음 | Store 경유 배포만. MSI/EXE로 제출하면 직접 서명해야 함 |
| Azure Artifact Signing(옛 Trusted Signing) | 약 $9.99/월 | **불가: 개인은 미국·캐나다만**. 조직은 미국·캐나다·EU·영국 | 평판을 쌓아야 함 | CI 연동 쉬움 |
| OV 인증서 | $150–300/년, HSM이나 토큰 필요 | 가능 | 평판을 쌓아야 함 | |
| EV 인증서 | $400+/년 | 가능 | 2024년부터 OV와 같음(즉시 통과 혜택 없어짐) | 권장하지 않음 |
| **SignPath Foundation** [SP1] [SP2] | **오픈소스 무료** | 가능(국가 제한 문구를 찾지 못함) | 평판을 쌓아야 함 | 조건이 있음(아래) |

   SignPath Foundation 조건 [SP2]:
   - 모든 구성요소가 OSI 승인 라이선스이고, 상업용 이중 라이선스가 없어야 한다.
   - 독점 구성요소가 없어야 한다.
   - 검증 가능한 방식으로 소스에서 빌드해야 하고, 릴리스마다 수동 승인한다.
   - 팀원 전원이 MFA를 쓴다.
   - 웹사이트에 "Code signing policy" 섹션을 둔다.
   - 사용자가 지정하지 않은 시스템으로 데이터를 보내면 개인정보 처리방침을 두고, 설치 시 표시하고, 끄는 옵션을 준다. AI 기능은 사용자가 고른 제공자로만 보내고, 이 점을 문서화한다.
   - 시스템 설정을 바꾸면 경고한다.
   - 설치 기능이 있으면 제거 기능도 준다.

### 3.7 한국 저작권법

현행 저작권법은 법률 제21336호(2026. 2. 10. 일부개정, 2026. 8. 11. 시행)다 [KR1]. 이번 개정이 아래 제101조의2~4를 바꿨는지는 확인하지 못했다. 조문 본문은 law.go.kr 화면이 동적으로 렌더링되어 케이스노트 전재본으로 확인했다.

- **제101조의2(보호의 대상)**: 프로그램 언어, 규약, 해법(지시·명령의 조합 방법)에는 저작권법을 적용하지 않는다 [KR4]. MFT 스캔 방식, USN 추적, 트리맵 배치 같은 알고리즘과 기능은 누구나 구현할 수 있다.
- **제101조의3 제1항 제6호**: 정당한 권한으로 프로그램을 이용하는 사람이 이용하는 중에, 프로그램의 기초인 아이디어와 원리를 확인하려고 기능을 조사·연구·시험하는 목적으로 복제할 수 있다. 저작재산권자의 이익을 부당하게 해치지 않아야 한다 [KR3]. 정품 WizTree를 쓰면서 블랙박스로 관찰하는 것은 이 범위에 가깝다.
- **제101조의4(프로그램코드역분석)** [KR2]
  - 제1항: 정당한 이용자가 호환에 필요한 정보를 쉽게 얻을 수 없고 얻는 것이 불가피할 때, 호환에 필요한 부분에 한해서만 허락 없이 역분석할 수 있다.
  - 제2항: 그렇게 얻은 정보를 호환 외 목적으로 쓰거나 제3자에게 주면 안 되고, **대상과 표현이 실질적으로 비슷한 프로그램을 개발·판매하는 데 써도 안 된다.**
  - 결론: "WizTree를 디컴파일해 비슷한 프로그램을 만드는 것"은 호환 목적이 아니고 제2항 제2호에도 걸리므로 허용되지 않는다. EULA도 금지한다 [WT5].
  - 2차 자료에 따르면 서울중앙지법 2013가합23162 판결은 제101조의4를 호환 목적 역분석에 관한 특별 규정으로 보았다(2차, 원문 미확인). 역분석 금지 계약의 효력은 학설이 갈린다(2차).
- 제104조의2(기술적 보호조치 무력화 금지)는 예외로 "호환을 위해 필요한 범위의 프로그램코드역분석"을 둔다 [KR1]. file-atlas는 다른 프로그램의 보호조치를 다루지 않으므로 직접 관련은 없다.
- **클린룸 재구현은 괜찮은가: 괜찮다(조건부 가능).** 조건:
  - (1) 다른 프로그램의 바이너리나 소스를 보지 않는다.
  - (2) Microsoft 공개 문서, 공개 crate, 자기 PC의 자기 파일로 한 실험만 근거로 쓴다.
  - (3) UI 아이콘과 그림은 직접 만든다.
  - (4) 개발 기록(참고 문서 목록, 커밋 이력)을 남겨 독자적으로 만들었다는 것을 증명할 수 있게 한다.

---

## 4. 확인하지 못했거나 출처가 서로 다른 점

1. **Anthropic 2026년 2월 문구**: "Agent SDK를 포함한 다른 제품에서 OAuth를 쓰면 위반"이라는 원문은 2차 기사로만 확인했다 [SEC1] [SEC2]. 웹 아카이브 접근이 막혀 있었다. 현행 원문 [AN1]은 그 문장이 없는 재작성본이다. Commercial Terms와 Usage Policy 본문은 이번에 직접 읽지 않았다(링크만 확인) [AN3] [AN4].
2. **Claude Code CLI를 오픈소스 앱이 호출하는 경우**가 [AN1]의 "제품에서 Claude Code 실행"(Commercial Terms 필요)에 해당하는지 원문만으로는 판단할 수 없다. `--bare`가 `-p`의 기본값이 되는 시점도 공지되지 않았다 [AN5].
3. **OpenAI Sign in with ChatGPT**: 단서는 "DevDay(2026-09-29)에 파트너 앱만"이라고 했다. 원문은 "상업 파트너는 제한 시범, 플랜 사용은 모든 오픈소스 파트너"로 되어 있다 [OA2]. 오픈소스 승인 절차와 대상 플랜(Go 포함 여부)은 확인 불가. 2차 기사끼리도 엇갈린다 [SEC3].
4. **OpenAI 이용약관 현행 원문**은 openai.com이 403을 돌려줘 직접 읽지 못했다. 2024-12-11판 미러 [OA6]로 자격증명 공유 금지와 자동 추출 금지를 확인했다. 검색 결과상 현행판은 2026-01-01 시행이다(직접 대조는 못 함) [OA5].
5. **TypeSafe**:
   - Master Customer Agreement가 공개되어 있지 않다.
   - docs의 "Legal" 페이지 내용을 확인하지 못했다.
   - Terms의 "Last updated Sep 19, 2026"과 페이지 메타데이터(Oct 19, 2026)가 다르다 [TSA2].
   - jevapi.org는 TypeSafe와의 관계가 불명확한 제3자 사이트다(2차) [SEC4].
6. **Files 라이선스**: MIT와 MPL-2.0이 섞여 있다(2026-06-30부터). 앞으로 어떤 파일이 MPL로 바뀔지는 파일 헤더를 하나씩 봐야 한다 [FIL6] [FIL7]. Store 가격은 확인 불가.
7. **WinDirStat**: 2.9.1 Beta(태그 없음)의 라이선스는 확인 불가. 2.9.0은 GPL v2, 2.9.3은 GPL v3+로 확인했다 [WDS4] [WDS1].
8. **릴리스 날짜 충돌**: OneCommander 3.108.0.0(2026-09-26과 2026-03-06) [OC1] [OC2]. Everything 1.5 최신 빌드(1424b와 1423b) [EV2] [EV3].
9. **가격 미확인**: Directory Opus 초기 가격(A$89는 2차 기사), FreeCommander 64비트 가격(404), Files Store 가격.
10. **Everything SDK 라이선스**는 2018년 포럼 답변 기준이다. SDK zip 안의 정식 라이선스 파일은 보지 않았다 [EV7].
11. **상표**: WizTree, Everything, voidtools의 한국 상표 등록 여부(KIPRIS)는 조사하지 않았다.
12. **Azure Artifact Signing 대상 국가**: Microsoft 문서는 조직 기준 미국·캐나다·EU·영국, 개인 기준 미국·캐나다라고 한다 [MS10]. 2차 자료 중에는 12개국이라는 주장도 있다. SignPath가 관리자 헬퍼나 보호 파일 열람 도구를 "hacking tools"로 볼지는 확인 불가.
13. **Windows 탐색기의 G·K 열**(UAC 프롬프트, 디지털 서명 탭)은 일반적으로 알려진 동작이지만 이번에 출처를 확보하지 못했다. 탐색기의 Copilot 우클릭 동작 8개를 2026년 12월까지 제거한다는 보도는 2차 출처(Windows Latest)뿐이다 [WE5].
14. **소규모 저장소**(Glint, Ferret, AI File Explorer 등, 별 0~5개)의 기능은 README의 자체 주장이다. 코드는 검증하지 않았다.
15. **한국 법령 본문**: law.go.kr 조문 화면이 동적으로 렌더링되어, 제101조의2~4 본문은 케이스노트 전재본으로 확인했다 [KR2] [KR3] [KR4]. 2026년 개정(법률 제21336호)이 이 조문들을 바꿨는지는 확인 불가 [KR1].
16. **Microsoft Learn 문서 라이선스**: Learn 이용약관(개인·비상업 사용)과 MicrosoftDocs/win32 GitHub의 CC-BY-4.0·MIT 표기가 서로 다르다. 어느 쪽이 코드 샘플에 적용되는지 확인 불가 [MS7] [MS8].
17. SpaceSniffer 최신 릴리스 날짜와 상업적 이용 조건은 확인 불가 [SS1].

---

## 5. 출처 목록

### Microsoft (Windows API, 문서, 서명, Defender)
- **MS1**: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data>
- **MS2**: <https://learn.microsoft.com/en-us/windows/win32/fileio/change-journals>
- **MS3**: <https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_get_ntfs_file_record>
- **MS4**: <https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew>
- **MS5**: <https://learn.microsoft.com/en-us/windows/win32/fileio/obtaining-a-volume-handle-for-change-journal-operations>
- **MS6**: <https://learn.microsoft.com/en-us/windows/win32/fileio/master-file-table>
- **MS7**: <https://learn.microsoft.com/en-us/legal/termsofuse>
- **MS8**: <https://github.com/MicrosoftDocs/win32>
- **MS9**: <https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights>
- **MS10**: <https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options>
- **MS11**: <https://learn.microsoft.com/en-us/defender-endpoint/defender-endpoint-false-positives-negatives>
- **MS12**: <https://www.microsoft.com/wdsi/filesubmission/>
- **WE1**: <https://blogs.windows.com/windowsexperience/2022/10/18/making-the-everyday-easier-with-new-experiences-available-in-windows-11/>
- **WE2**: <https://blogs.windows.com/windowsexperience/2025/10/16/new-experiences-currently-rolling-out-for-windows-11/>
- **WE3**: <https://learn.microsoft.com/en-us/windows/ai/mcp/file-connector>
- **WE4**: <https://learn.microsoft.com/en-us/windows/whats-new/whats-new-windows-11-version-26h2>
- **WE5**: <https://www.windowslatest.com/2026/10/21/microsoft-is-now-scaling-back-copilot-in-windows-11-amid-microslop-backlash-pulling-8-ai-actions-from-file-explorer-and-calling-it-simplifying/> (2차)
- **WE6**: <https://support.microsoft.com/en-us/windows/keyboard-shortcuts-in-windows-dcc61a57-8ff0-cffe-9796-cb9706c75eec>
- **CPA**: <https://blogs.windows.com/windows-insider/2025/11/17/copilot-on-windows-copilot-actions-begins-rolling-out-to-windows-insiders/>

### 탐색기 대체
- **FIL1**: <https://files.community/>
- **FIL2**: <https://github.com/files-community/Files/releases>
- **FIL3**: <https://files.community/docs/customize-settings/files-and-folders>
- **FIL4**: <https://github.com/files-community/Files/issues/5845>
- **FIL5**: <https://github.com/files-community/Files> (루트의 LICENSE-MIT, LICENSE-MPL)
- **FIL6**: <https://github.com/files-community/Files/pull/18657> ("GitHub: Add MPL License", 2026-06-30)
- **FIL7**: <https://github.com/files-community/Files/blob/main/src/Files.App/Data/Models/VolumeInfo.cs> (MPL-2.0 헤더) / https://github.com/files-community/Files/blob/main/src/Files.App/Actions/IAction.cs (MIT 헤더)
- **FIL8**: <https://github.com/files-community/Files/blob/main/src/Files.App/Dialogs/ElevateConfirmDialog.xaml>
- **FIL9**: <https://github.com/files-community/Files/blob/main/src/Files.App/Actions/Global/UndoAction.cs>
- **FIL10**: <https://github.com/files-community/Files/blob/main/src/Files.App/Data/Models/SignatureInfoItem.cs>
- **FIL11**: <https://files.community/blog>
- **FIL12**: <https://files.community/docs/getting-started/install>
- **OC1**: <https://www.onecommander.com/>
- **OC2**: <https://www.onecommander.com/releasenotes.txt>
- **OC3**: <https://www.onecommander.com/pricing>
- **OC4**: <https://www.onecommander.com/beta>
- **OC5**: <https://onecommander.com/pro-features>
- **OC6**: <https://onecommander.com/help2/FolderPane.html>
- **DO1**: <https://resource.dopus.com/c/new-releases/>
- **DO2**: <https://docs.dopus.com/doku.php?id=basic_concepts>
- **DO3**: <https://docs.dopus.com/doku.php?id=file_operations:tracking_and_undoing_file_operations>
- **DO4**: <https://docs.dopus.com/doku.php?id=basic_concepts%3Athe_lister%3Acalculating_folder_sizes>
- **DO5**: <https://docs.dopus.com/doku.php?id=additional_functionality%3Aeverything_integration>
- **DO6**: <https://docs.dopus.com/doku.php?id=file_operations%3Auac_and_administrator_mode>
- **DO7**: <https://docs.dopus.com/doku.php?id=additional_functionality:making_links_and_junctions>
- **DO8**: <https://docs.dopus.com/doku.php?id=reference%3Acommand_reference%3Ainternal_commands%3Afind>
- **DO9**: <https://docs.dopus.com/doku.php?id=reference%3Ametadata_keywords%3Akeywords_for_columns>
- **DO10**: <https://docs.dopus.com/doku.php?id=reference%3Ascripting_reference%3Ascripting_objects%3Afsutil>
- **DO11**: <https://www.gpsoft.com.au/>
- **DO12**: <https://resource.dopus.com/t/upgrade-subscriptions/58445>
- **DO13**: <https://resource.dopus.com/t/ai-based-find-duplicate-files/50553>
- **XY1**: <https://www.xyplorer.com/>
- **XY2**: <https://www.xyplorer.com/product.php>
- **XY3**: <https://www.xyplorer.com/features.php>
- **XY4**: <https://www.xyplorer.com/release_28.40.php>
- **XY5**: <https://www.xyplorer.com/xyfc/viewtopic.php?p=188989>
- **TC1**: <https://www.ghisler.com/whatsnew.htm>
- **TC2**: <https://www.ghisler.com/order.htm>
- **TC3**: <https://www.ghisler.com/history.txt>
- **TC4**: <https://www.ghisler.com/featurel.htm>
- **TC5**: <https://www.ghisler.com/>
- **TC6**: <http://www.ghisler.com/history852.txt>
- **EPP1**: <https://github.com/derceg/explorerplusplus>
- **EPP2**: <https://github.com/derceg/explorerplusplus/blob/master/LICENSE>
- **EPP3**: <https://explorerplusplus.com/changelog>
- **EPP4**: <https://github.com/derceg/explorerplusplus/releases>
- **EPP5**: <https://github.com/derceg/explorerplusplus/blob/master/Documentation/User/Sphinx/menus/tools/options/files_and_folders.rst>
- **FC1**: <https://freecommander.com/en/downloads/>
- **FC2**: <https://freecommander.com/en/license/>
- **DC1**: <https://github.com/doublecmd/doublecmd>

### 디스크 분석, 검색
- **WT1**: <https://diskanalyzer.com/>
- **WT2**: <https://diskanalyzer.com/whats-new>
- **WT3**: <https://diskanalyzer.com/guide>
- **WT4**: <https://diskanalyzer.com/donate>
- **WT5**: <https://diskanalyzer.com/eula>
- **WT6**: <https://diskanalyzer.com/distribution-license>
- **WF1**: <https://antibody-software.com/wizfile/>
- **WDS1**: <https://github.com/windirstat/windirstat>
- **WDS2**: <https://github.com/windirstat/windirstat/releases>
- **WDS3**: <https://github.com/windirstat/windirstat/blob/master/CHANGELOG.md>
- **WDS4**: <https://github.com/windirstat/windirstat/blob/release/v2.9.0/README.md>
- **SS1**: <https://www.uderzo.it/main_products/space_sniffer/>
- **TS1**: <https://www.jam-software.com/treesize/changes.shtml>
- **TS2**: <https://www.jam-software.com/treesize_free/changes.shtml>
- **TS3**: <https://www.jam-software.com/treesize_free>
- **TS4**: <https://www.jam-software.com/treesize>
- **TS5**: <https://www.jam-software.com/treesize/features.shtml>
- **TS6**: <https://manuals.jam-software.com/treesize/EN/hometab.html>
- **EV1**: <https://www.voidtools.com/Changes.txt>
- **EV2**: <https://www.voidtools.com/forum/viewtopic.php?t=9787>
- **EV3**: <https://www.voidtools.com/everything-1.5/>
- **EV4**: <https://www.voidtools.com/License.txt>
- **EV5**: <https://www.voidtools.com/forum/viewtopic.php?t=6005>
- **EV6**: <https://www.voidtools.com/support/everything/sdk/>
- **EV7**: <https://www.voidtools.com/forum/viewtopic.php?t=6687>
- **EV8**: <https://www.voidtools.com/faq/>
- **EV9**: <https://www.voidtools.com/support/everything/indexes/>
- **EV10**: <https://www.voidtools.com/forum/viewtopic.php?t=9236>
- **US1**: <https://www.jam-software.com/ultrasearch/changes.shtml>
- **US2**: <https://www.jam-software.com/ultrasearch>
- **US3**: <https://knowledgebase.jam-software.com/7580>
- **SW1**: <https://sourceforge.net/projects/swiftsearch/>

### AI 정리 도구, 신생 프로젝트, MCP
- **AFS1**: <https://github.com/hyperfield/ai-file-sorter>
- **AFS2**: <https://github.com/hyperfield/ai-file-sorter/releases>
- **AFS3**: <https://apps.microsoft.com/detail/9npk4dzd6r6s>
- **LFO1**: <https://github.com/QiuYannnn/Local-File-Organizer>
- **LFS1**: <https://github.com/iyaja/llama-fs>
- **FWA1**: <https://github.com/AIxHunter/FileWizardAI>
- **SOR1**: <https://www.getsortio.com/>
- **RC1**: <https://rename.click/>
- **RN1**: <https://renamer.ai/>
- **NQ1**: <https://www.namequick.app/>
- **ADC**: <https://github.com/vudsen/ai-disk-cleaner>
- **AFE**: <https://github.com/Jesusfloww/AI-File-Explorer>
- **CC1**: <https://www.ccleaner.com/ccleaner/version-history>
- **SD1**: <https://github.com/spacedriveapp/spacedrive>
- **SD2**: <https://github.com/spacedriveapp/spacedrive/releases>
- **XP1**: <https://github.com/kimlimjustin/xplorer>
- **XP2**: <https://xplorer.space/>
- **SFM**: <https://github.com/aleksey-hoffman/sigma-file-manager>
- **GL1**: <https://github.com/shadijhade/glint>
- **GL2**: <https://github.com/shadijhade/glint/pull/3>
- **FE1**: <https://github.com/meltingice1337/ferret>
- **LT1**: <https://github.com/Lertaro/Lertaro>
- **OS1**: <https://github.com/Eul45/omni-search>
- **MCPFS**: <https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem>
- **MCPEV**: <https://github.com/mamertofabian/mcp-everything-search>
- **MCPL**: <https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/LICENSE>
- **COW**: <https://academy.claude.com/use-cases/organize-files-by-whats-in-them>

### Rust crate (crates.io)
- **CR1**: <https://crates.io/crates/ntfs>
- **CR2**: <https://crates.io/crates/mft>
- **CR3**: <https://crates.io/crates/usn-journal-rs>
- **CR4**: <https://crates.io/crates/windows>
- **CR5**: <https://crates.io/crates/windows-sys>
- **CR6**: <https://crates.io/crates/tauri>
- **CR7**: <https://crates.io/crates/rmcp>
- **CR8**: <https://crates.io/crates/ntfs-reader>
- **CR9**: <https://crates.io/crates/trash>
- **CR10**: <https://crates.io/crates/junction>
- **CR11**: <https://crates.io/crates/usn-parser>
- **CR12**: <https://crates.io/crates/tauri-plugin-shell>
- **GNU1**: <https://www.gnu.org/licenses/license-list.html>

### AI 제공자 약관
- **AN1**: <https://code.claude.com/docs/en/legal-and-compliance> (2026-10-21 열람)
- **AN2**: <https://www.anthropic.com/legal/consumer-terms> (2025-10-08 시행판)
- **AN3**: <https://www.anthropic.com/legal/commercial-terms> (본문 미열람)
- **AN4**: <https://www.anthropic.com/legal/aup> (본문 미열람)
- **AN5**: <https://code.claude.com/docs/en/headless>
- **AN6**: <https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>
- **AN7**: <https://github.com/anthropics/claude-code/blob/main/LICENSE.md>
- **OA1**: <https://learn.chatgpt.com/docs/auth> (developers.openai.com/codex/auth에서 리디렉트)
- **OA2**: <https://developers.openai.com/siwc/quickstart>
- **OA3**: <https://developers.openai.com/siwc/token-sharing-open-source/sign-in>
- **OA4**: <https://help.openai.com/en/articles/20001410-sign-in-with-chatgpt> (검색 요약으로만 확인)
- **OA5**: <https://openai.com/policies/row-terms-of-use/> (403으로 직접 열람 못 함)
- **OA6**: <https://open.windriver.com/info/uni-license-list/licenses/openai-tou-20241211.html> (2024-12-11판 미러)
- **OA7**: <https://github.com/openai/codex> (Apache-2.0)
- **TSA1**: <https://typesafe.ai/>
- **TSA2**: <https://typesafe.ai/legal/terms>
- **TSA3**: <https://typesafe.ai/legal/acceptable-use-policy>
- **TSA4**: <https://typesafe.ai/legal/privacy-policy>
- **TSA5**: <https://docs.typesafe.ai/introduction/quickstart>
- **TSA6**: <https://docs.typesafe.ai/llms.txt>
- **SEC1**: <https://www.theregister.com/software/2026/02/20/anthropic-clarifies-ban-on-third-party-tool-access-to-claude/5014546> (2차)
- **SEC2**: <https://autonomee.ai/blog/claude-code-terms-of-service-explained/> (2차, 2026-10-07 갱신)
- **SEC3**: <https://thenewstack.io/sign-in-with-chatgpt/> (2차)
- **SEC4**: <https://jevapi.org/> (2차, 관계 불명)

### 코드 서명
- **SP1**: <https://signpath.org/>
- **SP2**: <https://signpath.org/terms>

### 한국 법령
- **KR1**: <https://www.law.go.kr/lsLawLinkInfo.do?lsJoLnkSeq=1000979096&chrClsCd=010202> (제104조의2, 법률 제21336호 표기)
- **KR2**: <https://casenote.kr/법령/저작권법/제101조의4> (조문 전재)
- **KR3**: <https://casenote.kr/법령/저작권법/제101조의3> (조문 전재)
- **KR4**: <https://casenote.kr/법령/저작권법/제101조의2> (조문 전재)
- **KR5**: <https://www.law.go.kr/법령/저작권법> (원 소관 출처, 본문은 동적 렌더링)
- **KR6**: <https://elaw.klri.re.kr/kor_service/lawViewTitle.do?hseq=16230> (영문 법령 목차)

<!-- 표 안의 [ID] 링크 정의 -->
[MS1]: https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_enum_usn_data
[MS2]: https://learn.microsoft.com/en-us/windows/win32/fileio/change-journals
[MS3]: https://learn.microsoft.com/en-us/windows/win32/api/winioctl/ni-winioctl-fsctl_get_ntfs_file_record
[MS4]: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew
[MS5]: https://learn.microsoft.com/en-us/windows/win32/fileio/obtaining-a-volume-handle-for-change-journal-operations
[MS6]: https://learn.microsoft.com/en-us/windows/win32/fileio/master-file-table
[MS7]: https://learn.microsoft.com/en-us/legal/termsofuse
[MS8]: https://github.com/MicrosoftDocs/win32
[MS9]: https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
[MS10]: https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options
[MS11]: https://learn.microsoft.com/en-us/defender-endpoint/defender-endpoint-false-positives-negatives
[MS12]: https://www.microsoft.com/wdsi/filesubmission/
[WE1]: https://blogs.windows.com/windowsexperience/2022/10/18/making-the-everyday-easier-with-new-experiences-available-in-windows-11/
[WE2]: https://blogs.windows.com/windowsexperience/2025/10/16/new-experiences-currently-rolling-out-for-windows-11/
[WE3]: https://learn.microsoft.com/en-us/windows/ai/mcp/file-connector
[WE4]: https://learn.microsoft.com/en-us/windows/whats-new/whats-new-windows-11-version-26h2
[WE5]: https://www.windowslatest.com/2026/10/21/microsoft-is-now-scaling-back-copilot-in-windows-11-amid-microslop-backlash-pulling-8-ai-actions-from-file-explorer-and-calling-it-simplifying/
[WE6]: https://support.microsoft.com/en-us/windows/keyboard-shortcuts-in-windows-dcc61a57-8ff0-cffe-9796-cb9706c75eec
[CPA]: https://blogs.windows.com/windows-insider/2025/11/17/copilot-on-windows-copilot-actions-begins-rolling-out-to-windows-insiders/
[FIL1]: https://files.community/
[FIL2]: https://github.com/files-community/Files/releases
[FIL3]: https://files.community/docs/customize-settings/files-and-folders
[FIL4]: https://github.com/files-community/Files/issues/5845
[FIL5]: https://github.com/files-community/Files
[FIL6]: https://github.com/files-community/Files/pull/18657
[FIL7]: https://github.com/files-community/Files/blob/main/src/Files.App/Data/Models/VolumeInfo.cs
[FIL8]: https://github.com/files-community/Files/blob/main/src/Files.App/Dialogs/ElevateConfirmDialog.xaml
[FIL9]: https://github.com/files-community/Files/blob/main/src/Files.App/Actions/Global/UndoAction.cs
[FIL10]: https://github.com/files-community/Files/blob/main/src/Files.App/Data/Models/SignatureInfoItem.cs
[FIL11]: https://files.community/blog
[FIL12]: https://files.community/docs/getting-started/install
[OC1]: https://www.onecommander.com/
[OC2]: https://www.onecommander.com/releasenotes.txt
[OC3]: https://www.onecommander.com/pricing
[OC4]: https://www.onecommander.com/beta
[OC5]: https://onecommander.com/pro-features
[OC6]: https://onecommander.com/help2/FolderPane.html
[DO1]: https://resource.dopus.com/c/new-releases/
[DO2]: https://docs.dopus.com/doku.php?id=basic_concepts
[DO3]: https://docs.dopus.com/doku.php?id=file_operations:tracking_and_undoing_file_operations
[DO4]: https://docs.dopus.com/doku.php?id=basic_concepts%3Athe_lister%3Acalculating_folder_sizes
[DO5]: https://docs.dopus.com/doku.php?id=additional_functionality%3Aeverything_integration
[DO6]: https://docs.dopus.com/doku.php?id=file_operations%3Auac_and_administrator_mode
[DO7]: https://docs.dopus.com/doku.php?id=additional_functionality:making_links_and_junctions
[DO8]: https://docs.dopus.com/doku.php?id=reference%3Acommand_reference%3Ainternal_commands%3Afind
[DO9]: https://docs.dopus.com/doku.php?id=reference%3Ametadata_keywords%3Akeywords_for_columns
[DO10]: https://docs.dopus.com/doku.php?id=reference%3Ascripting_reference%3Ascripting_objects%3Afsutil
[DO11]: https://www.gpsoft.com.au/
[DO12]: https://resource.dopus.com/t/upgrade-subscriptions/58445
[DO13]: https://resource.dopus.com/t/ai-based-find-duplicate-files/50553
[XY1]: https://www.xyplorer.com/
[XY2]: https://www.xyplorer.com/product.php
[XY3]: https://www.xyplorer.com/features.php
[XY4]: https://www.xyplorer.com/release_28.40.php
[XY5]: https://www.xyplorer.com/xyfc/viewtopic.php?p=188989
[TC1]: https://www.ghisler.com/whatsnew.htm
[TC2]: https://www.ghisler.com/order.htm
[TC3]: https://www.ghisler.com/history.txt
[TC4]: https://www.ghisler.com/featurel.htm
[TC5]: https://www.ghisler.com/
[TC6]: http://www.ghisler.com/history852.txt
[EPP1]: https://github.com/derceg/explorerplusplus
[EPP2]: https://github.com/derceg/explorerplusplus/blob/master/LICENSE
[EPP3]: https://explorerplusplus.com/changelog
[EPP4]: https://github.com/derceg/explorerplusplus/releases
[EPP5]: https://github.com/derceg/explorerplusplus/blob/master/Documentation/User/Sphinx/menus/tools/options/files_and_folders.rst
[FC1]: https://freecommander.com/en/downloads/
[FC2]: https://freecommander.com/en/license/
[DC1]: https://github.com/doublecmd/doublecmd
[WT1]: https://diskanalyzer.com/
[WT2]: https://diskanalyzer.com/whats-new
[WT3]: https://diskanalyzer.com/guide
[WT4]: https://diskanalyzer.com/donate
[WT5]: https://diskanalyzer.com/eula
[WT6]: https://diskanalyzer.com/distribution-license
[WF1]: https://antibody-software.com/wizfile/
[WDS1]: https://github.com/windirstat/windirstat
[WDS2]: https://github.com/windirstat/windirstat/releases
[WDS3]: https://github.com/windirstat/windirstat/blob/master/CHANGELOG.md
[WDS4]: https://github.com/windirstat/windirstat/blob/release/v2.9.0/README.md
[SS1]: https://www.uderzo.it/main_products/space_sniffer/
[TS1]: https://www.jam-software.com/treesize/changes.shtml
[TS2]: https://www.jam-software.com/treesize_free/changes.shtml
[TS3]: https://www.jam-software.com/treesize_free
[TS4]: https://www.jam-software.com/treesize
[TS5]: https://www.jam-software.com/treesize/features.shtml
[TS6]: https://manuals.jam-software.com/treesize/EN/hometab.html
[EV1]: https://www.voidtools.com/Changes.txt
[EV2]: https://www.voidtools.com/forum/viewtopic.php?t=9787
[EV3]: https://www.voidtools.com/everything-1.5/
[EV4]: https://www.voidtools.com/License.txt
[EV5]: https://www.voidtools.com/forum/viewtopic.php?t=6005
[EV6]: https://www.voidtools.com/support/everything/sdk/
[EV7]: https://www.voidtools.com/forum/viewtopic.php?t=6687
[EV8]: https://www.voidtools.com/faq/
[EV9]: https://www.voidtools.com/support/everything/indexes/
[EV10]: https://www.voidtools.com/forum/viewtopic.php?t=9236
[US1]: https://www.jam-software.com/ultrasearch/changes.shtml
[US2]: https://www.jam-software.com/ultrasearch
[US3]: https://knowledgebase.jam-software.com/7580
[SW1]: https://sourceforge.net/projects/swiftsearch/
[AFS1]: https://github.com/hyperfield/ai-file-sorter
[AFS2]: https://github.com/hyperfield/ai-file-sorter/releases
[AFS3]: https://apps.microsoft.com/detail/9npk4dzd6r6s
[LFO1]: https://github.com/QiuYannnn/Local-File-Organizer
[LFS1]: https://github.com/iyaja/llama-fs
[FWA1]: https://github.com/AIxHunter/FileWizardAI
[SOR1]: https://www.getsortio.com/
[RC1]: https://rename.click/
[RN1]: https://renamer.ai/
[NQ1]: https://www.namequick.app/
[ADC]: https://github.com/vudsen/ai-disk-cleaner
[AFE]: https://github.com/Jesusfloww/AI-File-Explorer
[CC1]: https://www.ccleaner.com/ccleaner/version-history
[SD1]: https://github.com/spacedriveapp/spacedrive
[SD2]: https://github.com/spacedriveapp/spacedrive/releases
[XP1]: https://github.com/kimlimjustin/xplorer
[XP2]: https://xplorer.space/
[SFM]: https://github.com/aleksey-hoffman/sigma-file-manager
[GL1]: https://github.com/shadijhade/glint
[GL2]: https://github.com/shadijhade/glint/pull/3
[FE1]: https://github.com/meltingice1337/ferret
[LT1]: https://github.com/Lertaro/Lertaro
[OS1]: https://github.com/Eul45/omni-search
[MCPFS]: https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem
[MCPEV]: https://github.com/mamertofabian/mcp-everything-search
[MCPL]: https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/LICENSE
[COW]: https://academy.claude.com/use-cases/organize-files-by-whats-in-them
[CR1]: https://crates.io/crates/ntfs
[CR2]: https://crates.io/crates/mft
[CR3]: https://crates.io/crates/usn-journal-rs
[CR4]: https://crates.io/crates/windows
[CR5]: https://crates.io/crates/windows-sys
[CR6]: https://crates.io/crates/tauri
[CR7]: https://crates.io/crates/rmcp
[CR8]: https://crates.io/crates/ntfs-reader
[CR9]: https://crates.io/crates/trash
[CR10]: https://crates.io/crates/junction
[CR11]: https://crates.io/crates/usn-parser
[CR12]: https://crates.io/crates/tauri-plugin-shell
[GNU1]: https://www.gnu.org/licenses/license-list.html
[AN1]: https://code.claude.com/docs/en/legal-and-compliance
[AN2]: https://www.anthropic.com/legal/consumer-terms
[AN3]: https://www.anthropic.com/legal/commercial-terms
[AN4]: https://www.anthropic.com/legal/aup
[AN5]: https://code.claude.com/docs/en/headless
[AN6]: https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop
[AN7]: https://github.com/anthropics/claude-code/blob/main/LICENSE.md
[OA1]: https://learn.chatgpt.com/docs/auth
[OA2]: https://developers.openai.com/siwc/quickstart
[OA3]: https://developers.openai.com/siwc/token-sharing-open-source/sign-in
[OA4]: https://help.openai.com/en/articles/20001410-sign-in-with-chatgpt
[OA5]: https://openai.com/policies/row-terms-of-use/
[OA6]: https://open.windriver.com/info/uni-license-list/licenses/openai-tou-20241211.html
[OA7]: https://github.com/openai/codex
[TSA1]: https://typesafe.ai/
[TSA2]: https://typesafe.ai/legal/terms
[TSA3]: https://typesafe.ai/legal/acceptable-use-policy
[TSA4]: https://typesafe.ai/legal/privacy-policy
[TSA5]: https://docs.typesafe.ai/introduction/quickstart
[TSA6]: https://docs.typesafe.ai/llms.txt
[SEC1]: https://www.theregister.com/software/2026/02/20/anthropic-clarifies-ban-on-third-party-tool-access-to-claude/5014546
[SEC2]: https://autonomee.ai/blog/claude-code-terms-of-service-explained/
[SEC3]: https://thenewstack.io/sign-in-with-chatgpt/
[SEC4]: https://jevapi.org/
[SP1]: https://signpath.org/
[SP2]: https://signpath.org/terms
[KR1]: https://www.law.go.kr/lsLawLinkInfo.do?lsJoLnkSeq=1000979096&chrClsCd=010202
[KR2]: https://casenote.kr/법령/저작권법/제101조의4
[KR3]: https://casenote.kr/법령/저작권법/제101조의3
[KR4]: https://casenote.kr/법령/저작권법/제101조의2
[KR5]: https://www.law.go.kr/법령/저작권법
[KR6]: https://elaw.klri.re.kr/kor_service/lawViewTitle.do?hseq=16230
