# 아키텍처

## 현재 상태

- `prototype/python/`에는 전체 124권 생성과 검증에 사용한 Python 기준선이
  있습니다.
- Rust 주 구현은 KDEP-005 전체 코퍼스 자동 검증까지 진행됐습니다.
  `preflight`는 경로와 실행 정책만 점검하고, `inspect`는 추적 XML 한 권의
  항목 수와 digest를 출력하며, `build`는 선택한 한 권을 EPUB 3로 생성하고
  `audit`은 원본과 EPUB을 별도 코드 경로로 대조합니다. `batch`는 선택한
  catalog를 파일 단위로 생성·감사하고 선택적으로 EPUBCheck를 실행합니다.
- Rust 2024 edition과 MSRV 1.95를 사용합니다. MSRV 변경 근거는
  [`ADR-0011`](decisions/0011-rust-1-95-msrv.md)에 기록합니다.
- `references/korean-dict-nikl`은 읽기 전용 입력으로 취급할 Git 서브모듈입니다.
- 생성 EPUB과 보고서는 로컬 산출물이며 Git 추적 대상이 아닙니다.

웹사전은 같은 저장소의 후속 제품 트랙입니다. 기존 EPUB CLI와 실행
바이너리를 분리하고, 공통 XML 입력·보존·감사 코드는 저장소 안에서
공유합니다. KWEB-003의 SQLite schema·migration·read-only 검증 API가
구현됐으며 XML importer, 실행 바이너리와 브라우저 QA는 아직 시작하지
않았습니다.

## 목표 흐름

```text
references/korean-dict-nikl
  -> 입력 발견과 사전별 권 번호 계산
  -> 스트리밍 XML 이벤트
  -> 손실 없는 공통 SourceRecord
  -> 의미 강조가 더해진 XHTML 장
  -> EPUB 3 패키징
  -> 내부 manifest / digest
  -> 독립 원본-EPUB 감사
  -> EPUBCheck / 재열기 / 기기 QA
```

## 핵심 경계

### 원본 데이터

- 서브모듈 커밋이 입력 버전을 결정합니다.
- 부모 저장소의 변환기는 서브모듈 파일을 수정하지 않습니다.
- upstream 저장소의 미추적 파일이나 생성물은 입력 목록에 포함하지 않습니다.
- 입력 발견은 허용된 사전 디렉터리의 추적 XML만 대상으로 하고, 파일명 정렬을
  명시적으로 적용합니다.
- 현재 catalog는 shell을 거치지 않고 `git ls-files -z`를 실행합니다. Git
  전역 설정을 수정하지 않으며 현재 프로세스의 `safe.directory`만 canonical
  입력 경로로 지정합니다. 결과 경로는 사전 디렉터리 바로 아래의 `.xml`
  일반 파일인지 다시 검증하고 심볼릭 링크를 거부합니다.

### 보존 모델

Rust 구현의 중심 모델은 사전별 도메인 객체가 아니라 일반 XML 레코드입니다.
최소한 다음 정보를 순서대로 표현해야 합니다.

- 시작 요소의 전체 이름과 깊이
- 원본 순서의 속성명·값
- 의미 있는 요소 텍스트
- 빈 요소
- 의미 있는 tail 텍스트
- 파서가 직접 수용할 수 없는 바이트의 역변환 가능한 대체 표현

현재 `SourceRecord`는 시작 요소, 빈 요소, 요소 텍스트, tail 텍스트와 종료
요소를 별도 variant로 표현합니다. 속성은 파서가 읽은 원래 순서를 유지하며,
QName은 prefix를 포함한 입력 이름을 보존합니다. XML 1.0 금지 제어 바이트는
파싱 전에 BMP PUA escape로 치환한 뒤 레코드 값에서 복원합니다. 실제 입력의
escape 문자 자체는 두 번 기록해 제어문자 표식과 충돌하지 않게 합니다.

canonical digest v1은 다음 순서로 SHA-256에 입력합니다.

1. 고정 preamble `korean-dict-epub/source-record-digest/v1\0`
2. variant별 1바이트 tag
3. 깊이·문자열 길이·속성 수를 big-endian `u64`로 기록
4. 이름과 값을 UTF-8 byte로 기록

이 digest는 XML의 공백·인용부호 같은 lexical byte 동일성을 주장하지 않고,
요구사항에 정의된 논리 레코드와 순서의 동일성을 검증합니다.

표제어, 뜻풀이, 예문, 번역 같은 사전 지식은 CSS 클래스와 화면 제목을 만드는
보조 계층에서만 사용합니다. 사전별 매핑이 누락돼도 원본 데이터가 사라져서는
안 됩니다.

### EPUB 작성

- 권 하나는 여러 XHTML 장으로 나뉩니다.
- 장 분할은 항목 수와 직렬화된 바이트 크기 중 먼저 도달한 기준을 사용합니다.
- `mimetype`은 압축하지 않고 ZIP 첫 항목으로 기록합니다.
- OPF manifest, spine, nav와 실제 ZIP 항목은 서로 대조합니다.
- 권 제목, 시리즈, 권 번호와 식별자는 원본과 설정에서 결정적으로 계산합니다.
- 임시 파일에 완성한 뒤 최종 경로로 교체하여 부분 EPUB을 남기지 않습니다.

현재 renderer는 source record마다 독립된 XHTML block과
`data-kdep-kind`, `data-kdep-depth`를 기록합니다. 시작·빈·종료 요소,
element/tail 텍스트와 원래 순서의 속성은 모두 눈에 보이며, XML 금지
제어문자는 `data-codepoint` span으로 직렬화합니다. 알려진 표제어·뜻풀이·예문·
번역은 CSS class만 더합니다.

builder는 항목 하나만 메모리에 모아 표제어와 article을 만든 뒤 staging
XHTML에 기록합니다. 완결된 항목 수 300개 또는 직렬화 약 1MiB 중 먼저
도달한 지점에서 장을 닫으며 한 항목 내부는 나누지 않습니다. title, nav,
OPF, CSS와 chapter 목록을 확정한 다음 코드가 만든 고정 ZIP 경로만
패키징합니다.

ZIP timestamp와 OPF `dcterms:modified`는 1980-01-01로 고정합니다. UUID v5
식별자는 사전 key와 상대 XML 경로에서 계산합니다. 출력은 같은 디렉터리의
atomic temporary file에 완성·내부 점검한 뒤에만 최종 경로로 commit합니다.

### 검증 독립성

변환기의 자체 manifest만으로 원본 보존을 주장하지 않습니다.

1. 변환기는 원본 이벤트와 출력 레코드의 개수·digest를 기록합니다.
2. 별도 감사기는 원본 XML과 완성 EPUB을 다시 읽어 독립적으로 대조합니다.
3. EPUBCheck는 EPUB 표준 구조와 XHTML/OPF/nav/CSS를 검사합니다.
4. 일반 EPUB 도구 재열기와 실제 기기 QA는 소비자 관점의 검증을 담당합니다.

현재 감사기는 `source`·`record`·`render` 모듈을 호출하지 않습니다. 별도
제어문자 escape prefix와 streaming XML frame, canonical digest v1 encoder를
사용해 원본을 다시 읽습니다. 완성 EPUB은 OPF manifest/spine 순서로 XHTML을
열고 `data-kdep-record` markup에서 QName, 원래 순서의 속성, text/tail과
제어문자를 재구성합니다. title page에 기록된 digest는 비교 근거로 사용하지
않습니다.

권별 `kdep-audit-report-v1` JSON은 source/EPUB summary, package metadata,
check별 expected/actual 값과 재현 명령을 담고 같은 출력 디렉터리에 원자
교체됩니다. 내용 불일치는 `KDEP-E011`, 입력·구조·직렬화 오류는
`KDEP-E010`으로 구분합니다.

## 예정된 Rust 모듈

현재 구현:

| 모듈 | 책임 |
| --- | --- |
| `cli` | `preflight`·`inspect`·`build`·`audit` 명령과 안전 기본값 |
| `app` | 입력·출력 경계, 단권 검사, 오류 코드와 종료 정책 |
| `main` | 표준 출력·오류와 프로세스 종료 코드 연결 |
| `catalog` | Git 추적 XML, 사전·권 번호와 출력 파일명 |
| `source` | 제어 바이트 치환과 스트리밍 XML 이벤트 |
| `record` | 손실 없는 `SourceRecord`와 canonical digest v1 |
| `render` | generic record XHTML, 가시적 제어문자와 의미 CSS class |
| `epub` | 단권 장 분할, OPF·nav·CSS와 결정적 ZIP 패키징 |
| `audit` | 별도 원본 재독, EPUB record 복원, metadata 대조와 JSON 보고서 |
| `batch` | 파일 worker, 재개·중단 정책, EPUBCheck와 전체 통합 보고서 |
| `web_identity` | KWEB canonical entity·relation enum과 `kweb:v1/...` ID 생성 |
| `web_source_audit` | 전체 XML entity·관계 조사와 KWEB-002 보고서 |
| `web_db` | SQLite schema v1 생성·forward migration·read-only 검증 |

다음 이름은 이후 구현 방향이며 현재 파일이 존재한다는 뜻이 아닙니다.

| 모듈 | 책임 |
| --- | --- |
| `report` | 빌드·검증 JSON 보고서 |

## 성능과 실패 처리

- XML과 XHTML은 스트리밍 처리하여 권 크기에 비례한 전체 메모리 적재를
  피합니다.
- 병렬도는 파일 단위로 제한하고 사용자가 조절할 수 있게 합니다.
- 한 권 실패 시 기본 동작은 전체 명령 실패이며, 명시적 옵션에서만 다음 권을
  계속합니다.
- 이미 존재하는 출력은 기본적으로 덮어쓰지 않습니다.
- 실패한 권, 원인, 입력 파일과 검증 상태를 구조화된 보고서에 남깁니다.

현재 `batch`는 catalog 순서의 권을 공유 queue에서 최대 `--jobs` worker에
할당합니다. 각 worker는 한 권을 원자 생성한 다음 독립 감사를 통과시킨 뒤에만
EPUBCheck를 실행합니다. `--keep-going`이 없으면 첫 실패가 관찰된 뒤 새 권
할당을 중단하며, 이미 worker에 할당된 권은 마칠 수 있습니다.

기존 EPUB이 있는 기본 실행은 어떤 권도 만들기 전에 실패합니다. `--resume`은
기존 EPUB을 재생성하지 않고 독립 감사와 EPUBCheck를 다시 실행하며 누락된
권만 생성합니다. 통합 보고서는 catalog 순서로 정렬하므로 worker 완료 순서와
무관하게 안정적으로 읽을 수 있습니다.

## 도구 배포 구조

- 공개 릴리스는 Rust CLI 소스와 실행 바이너리, MIT License, 사용 문서와
  필요한 제3자 고지만 패키징합니다.
- 실행 바이너리에는 원본 XML이나 서브모듈 작업 트리를 내장하지 않습니다.
  사용자가 별도로 준비한 입력 경로를 읽는 구조를 유지합니다.
- 생성 EPUB과 검사 보고서는 사용자의 로컬 출력에만 기록하며, 네트워크
  업로드나 콘텐츠 게시 기능은 두지 않습니다.
- 릴리스 검증은 배포 파일 목록을 검사하여 XML, EPUB, 감사 보고서, 미디어,
  자격 증명과 장비별 경로가 포함되지 않았음을 확인합니다.
- 도구 릴리스와 전체 코퍼스 검증, 기기 QA, 생성 콘텐츠 배포는 서로 다른
  상태입니다. 생성 콘텐츠 배포는 이 프로젝트의 범위가 아닙니다.

## 로컬 웹사전 구조

저장소·바이너리·외부 로컬 자산 경계는
[`ADR-0009`](decisions/0009-local-web-app-boundary.md)를, schema와 migration은
[`ADR-0010`](decisions/0010-sqlite-schema-v1.md)을 따릅니다.

schema v1은 bundled SQLite를 사용하며 모든 application table을
`STRICT, WITHOUT ROWID`로 생성합니다. metadata·migration, corpus·source file,
canonical entity, lossless source record·attribute, 조회 projection, relation·raw
field·candidate를 분리합니다. native key는 `TEXT`로 보존하고 canonical ID와
원본 순서는 명시적 column·key로 저장하므로 `rowid`에 의존하지 않습니다.

내장 migration은 SQL checksum과 `PRAGMA user_version`을 함께 기록하고
`BEGIN IMMEDIATE` transaction으로 적용합니다. `web_db::validate`는 URI 해석
없는 read-only 연결에서 application ID, migration history, schema fingerprint,
`quick_check`와 foreign key를 확인합니다. `ReadyCorpus`는 단일 ready corpus와
source commit·필수 metadata·entity owner·대표 entry projection을 추가로
검사하며, 선택 파일을 복사하거나 migration하지 않습니다.

- 저장소는 하나로 유지하고 EPUB 변환 CLI와 로컬 웹사전 앱을 별도 Rust
  바이너리로 만듭니다.
- 웹사전 앱은 loopback HTTP server를 시작하고 바이너리에 내장된
  HTML·CSS·JavaScript·아이콘·글꼴을 제공합니다.
- importer가 생성한 SQLite corpus DB는 바이너리 밖의 로컬 파일입니다.
  웹사전 설정의 native file picker로 기존 파일을 선택하고, backend가
  SQLite 형식, schema version, 필수 metadata와 대표 조회를 검증한 뒤
  machine-local active DB 설정을 원자적으로 바꿉니다.
- DB 파일은 앱 전용 위치로 복사하지 않고 선택한 원래 경로에서 직접 엽니다.
  파일이 없거나 검증·적용이 실패하면 기존 active DB를 유지하고 조치 가능한
  오류를 표시합니다.
- KURE model과 `llama.cpp` runtime도 외부 로컬 자산이며 사용자의 PC 또는
  Mac에 이미 준비돼 있다고 가정합니다. 앱은 설치·다운로드·업데이트하지
  않습니다. vector index는 외부의 재생성 가능한 로컬 파생 파일입니다.
- DB 선택과 model/runtime 선택은 같은 설정 영역에서 제공할 수 있지만
  active 상태와 변경 수명주기는 분리합니다. model/runtime은 후보 검증,
  적용과 명시적 재색인을 서로 다른 동작으로 유지합니다.
- 웹사전의 network listener는 loopback에만 bind하며 공개 호스팅과 외부
  network service는 초기 구조에 포함하지 않습니다.

## 공개와 로컬 전용 경계

공개 추적 대상:

- 변환기와 테스트
- 로컬 웹사전 코드와 테스트
- Python 기준선
- README와 공개 요구사항·아키텍처·기술 결정
- `.gitmodules`와 서브모듈 gitlink

공개 릴리스 대상:

- Rust CLI 소스와 실행 바이너리
- MIT License, 사용 문서와 필요한 제3자 고지

로컬 전용:

- 에이전트 지침과 repo-local 스킬
- TODO, WBS, 티켓, 세션 근거
- 서브모듈 작업 트리, 원본에서 생성한 EPUB과 검사 보고서
- 캐시, 임시 파일, 자격 증명과 장비별 설정
