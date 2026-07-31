# 한국어 사전 독립출판

- 개인 독서용으로 구현 진행

국립국어원 사전 XML을 순차 독서용 EPUB 3 형식 전자책으로 변환하는 도구를 만드는
저장소입니다. 데이터 원본은 이 저장소에 복사하지 않고
`references/korean-dict-nikl` Git 서브모듈로 고정합니다.
후속 트랙으로 같은 XML을 SQLite DB에 적재해 브라우저에서 조회하는 로컬
웹사전을 계획합니다.

## 현재 상태

- Python 프로토타입은 `prototype/python/`에 있습니다.
- 직전 프로토타입 실행에서는 XML 124개를 EPUB 124권으로 변환하고,
  원본 대조·내부 무결성·EPUBCheck 검사를 124/124권 통과했습니다.
- Rust 주 구현은 단권 생성·독립 감사까지 진행됐습니다. `preflight`로
  입력·출력 경로와 정책을 점검하고 `inspect`로 추적 XML 한 권의 항목 수와
  canonical digest를 읽기 전용으로 확인할 수 있습니다. `build`는 선택한
  사전 한 권을 EPUB 3로 생성하고 `audit`은 원본과 EPUB을 별도 코드 경로로
  다시 읽어 비교합니다.
- Rust 전체 실행에서 XML/EPUB 124대 124, 총 1,697,692항목, 독립 감사
  124/124권과 EPUBCheck 5.3.0 경고·오류 0을 확인했습니다. 생성 EPUB과
  보고서는 로컬 산출물이며 저장소에는 포함하지 않습니다.
- 사용자는 Rust EPUB이 실제 리더에서 정상적으로 열리고 읽힌다고 판정했고,
  원본 XML 레코드가 전면에 보이는 현재 표현을 알려진 한계로 수용했습니다.
- 생성된 EPUB, 검사 보고서, 캐시와 원본 데이터 자체는 부모 저장소의 커밋
  대상이 아닙니다.
- 콘텐츠 없는 Rust CLI 공개 배포는 실행하지 않았으며 사용자 결정으로
  생략했습니다.
- 로컬 웹사전은 같은 저장소의 별도 Rust 바이너리로 계획됐고 아직 구현하지
  않았습니다.

현재 EPUB 트랙은 전체 코퍼스 검증과 사용자 수용까지 기록됐습니다. 공개
배포는 완료가 아니라 생략 상태이며, 다음 작업은 웹사전의 사전별 식별자·관계
필드 조사입니다.

## 확정된 범위

- 한국어기초사전 11개, 표준국어대사전 88개, 우리말샘 25개 XML을 다룹니다.
- XML 1개를 EPUB 1권으로 변환하며, 사전별 병합본은 만들지 않습니다.
- 원본 항목 순서와 모든 요소·속성·의미 있는 텍스트·빈 요소를 보존합니다.
- 관리 코드, 다국어 번역, URL도 제외하지 않습니다.
- 미디어 파일은 다운로드하거나 EPUB에 포함하지 않고 원본 URL만 보존합니다.
- 모든 본문에 `word-break: keep-all`을 적용합니다.
- 웹사전은 현재 저장소에서 EPUB CLI와 실행 바이너리만 분리하고 XML
  파싱·보존·감사 공통 코드를 공유합니다.
- 웹사전의 HTML·CSS·JavaScript·아이콘·글꼴은 앱 바이너리에 내장합니다.
- importer가 생성한 SQLite DB와 사용자의 PC 또는 Mac에 이미 준비된 KURE
  model·`llama.cpp` runtime은 외부 로컬 자산입니다. 웹사전 설정에서
  선택·검증·적용하고 앱은 다운로드·설치·업데이트하지 않습니다. vector
  index는 별도 외부 로컬 파일로 생성·관리합니다.

세부 기준은 [요구사항](docs/REQUIREMENTS.md), 구현 경계는
[아키텍처](docs/ARCHITECTURE.md)를 참고하십시오.

## 배포 경계

- 공개 배포는 현재 생략 상태입니다. 향후 재개한다면 공개 저장소와 릴리스에는
  변환기 소스, 실행 바이너리, 테스트, 사용 문서와 라이선스 고지만 포함합니다.
- 원본 XML, 서브모듈 작업 트리, 생성 EPUB, 검사 보고서와 미디어 파일은
  릴리스에 포함하지 않습니다. 공개 저장소에는 upstream을 가리키는 서브모듈
  포인터만 둡니다.
- 사용자는 원본 데이터를 별도로 준비하고 도구를 로컬에서 실행합니다.
- 도구는 생성 EPUB이나 보고서를 업로드·게시하는 기능을 제공하지 않습니다.
- 생성 콘텐츠의 공개 배포는 이 프로젝트의 범위가 아닙니다.

## 저장소 구성

```text
prototype/python/             검증된 Python 기준선
references/korean-dict-nikl/  원본 사전 Git 서브모듈
docs/REQUIREMENTS.md          확정 기준과 남은 결정
docs/ARCHITECTURE.md          변환 파이프라인과 경계
docs/decisions/               공개 기술 결정 기록
```

웹사전 코드가 구현되면 같은 저장소 안에서 EPUB CLI와 별도 실행
바이너리로 추가합니다. 저장소·바이너리·외부 로컬 자산 결정은
[ADR-0009](docs/decisions/0009-local-web-app-boundary.md)를 따릅니다.

에이전트 지침, 로컬 스킬, TODO, 티켓과 세션 근거 문서는 로컬 작업을 위해
존재할 수 있지만 `.gitignore`로 공개 커밋에서 제외합니다.

## 시작하기

원본 데이터가 크므로 서브모듈 초기화에는 약 3GB의 작업 트리 공간이 필요합니다.

```powershell
git clone --recurse-submodules https://github.com/john33fiao/korean-dict-epub.git
cd korean-dict-epub
python prototype/python/build_epubs.py --help
```

이미 저장소를 복제했다면 다음 명령으로 원본을 받습니다.

```powershell
git submodule update --init
```

Rust 1.85 이상에서 현재 CLI 사전 점검을 실행합니다.

```powershell
cargo run -- preflight
```

기본 입력은 `references/korean-dict-nikl`, 기본 출력은 `outputs/rust`입니다.
이 명령은 XML을 읽거나 출력 디렉터리를 만들지 않습니다. 현재 제공되는
`preflight`는 KDEP-001 실행 계약입니다.

Git으로 추적되는 XML 한 권의 손실 없는 레코드 digest를 확인할 수 있습니다.
이 명령은 Git이 필요하며 XML이나 출력 디렉터리를 수정하지 않습니다.

```powershell
cargo run -- inspect --dictionary krdict --volume 1
```

`--dictionary`는 `krdict`, `stdict`, `opendict` 중 하나이며 권 번호는 각
사전에서 파일명 순으로 계산합니다.

선택한 한 권을 기본 `outputs/rust` 디렉터리에 EPUB으로 생성합니다.

```powershell
cargo run --release -- build --dictionary krdict --volume 1
```

기존 파일은 기본적으로 덮어쓰지 않습니다. 의도적으로 교체할 때만
`--overwrite`를 사용합니다.

생성된 한 권을 원본과 독립적으로 대조하고 같은 출력 디렉터리에 결정적 JSON
보고서를 기록합니다.

```powershell
cargo run --release -- audit --dictionary krdict --volume 1
```

감사기는 변환기의 source reader, renderer와 자체 manifest digest를 증거로
재사용하지 않습니다. 필드 누락, 레코드·속성 순서, 속성 값과 제어문자 표식이
달라지면 실패합니다.

선택한 사전 또는 전체 코퍼스를 파일 단위로 생성·감사하고, EPUBCheck JAR를
지정한 경우 경고까지 실패로 검사합니다.

```powershell
cargo run --release -- batch --jobs 2 `
  --epubcheck-jar C:\path\to\epubcheck.jar
```

기본 실행은 기존 EPUB이 하나라도 있으면 시작 전에 거부합니다. 중단된 로컬
실행을 이어갈 때는 `--resume`, 전부 다시 만들 때는 `--overwrite`를
명시합니다. EPUBCheck를 지정하지 않은 성공 보고서는 `passed`가 아니라
`partial`입니다.

## 데이터와 저작권

서브모듈 데이터는 upstream 저장소의 CC BY-SA 2.0 KR 안내를 따릅니다.
표준국어대사전·우리말샘의 일부 인용 예문과 연결된 미디어에는 별도 이용
제한이 있을 수 있습니다. 이 프로젝트는 원본 데이터나 생성 EPUB을 공개
배포하지 않으며, 도구 사용자는 별도로 준비한 입력 데이터에 적용되는 조건을
확인해야 합니다.

이 부모 저장소의 변환기 코드와 문서는 [MIT License](LICENSE)로 공개합니다.
서브모듈 데이터와 그 파생물에는 upstream 데이터 라이선스와 원출처 조건이
별도로 적용됩니다.
