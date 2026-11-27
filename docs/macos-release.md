# Apple Silicon Mac 배포

macOS 번들 설정은 앱 아이콘, 프로젝트 MIT 라이선스, 제3자 고지를 `.app`에 포함하고 같은 앱을 담은 DMG를 만든다. 번들의 최소 OS 설정은 macOS 14.0이다. 지원을 안내할 때는 설치를 실제로 확인한 macOS 버전을 함께 적는다.

## 로컬 설치판 점검

Apple Silicon Mac에서 릴리스할 커밋을 체크아웃하고 저장소의 Node·npm 버전을 사용한다.

```bash
npm ci
npm run licenses:mac
git diff --exit-code -- licenses/MACOS-NOTICES.txt
CI=true APPLE_SIGNING_IDENTITY=- npm run bundle:mac
```

고지 생성기는 npm 운영 의존성·Tailwind CSS와 Apple Silicon용 Cargo 의존성을 확인한다. 패키지에 고지 파일이 없으면 지정된 upstream 커밋에서 원문을 가져온다. 잠금 파일을 바꾸면 고지의 변경분도 확인해 같은 릴리스에 커밋한다.

`.app`과 `.dmg`는 각각 `src-tauri/target/release/bundle/macos/`와 `src-tauri/target/release/bundle/dmg/`에 생긴다. 위 명령은 로컬 확인용 ad hoc 서명을 사용한다. Finder에서 앱의 **패키지 내용 보기 → Contents → Resources → licenses**를 열면 고지 원문을 볼 수 있다.

## GitHub에 설치판을 올릴 때

1. 배포할 커밋의 버전이 `package.json`, Cargo 패키지, Tauri 설정에서 같은지 확인한다. 빌드 Mac 키체인에서 유효한 **Developer ID Application** 인증서와 개인 키를 선택한다.
2. `APPLE_SIGNING_IDENTITY`를 해당 인증서 이름으로 설정하고 Tauri 공증용 인증 정보를 빌드 환경에 준비한다. `npm run bundle:mac`을 실행하고 앱 서명·공증 성공을 확인한다. 인증 정보 설정과 인증서 종류는 [Tauri macOS 서명 문서](https://v2.tauri.app/distribute/sign/macos/)를 따른다.
3. 아래 명령으로 최종 DMG를 서명하고 공증한다. `NOTARY_PROFILE`에는 이 Mac에 준비한 notarytool 키체인 프로필 이름을 지정한다. 제출 결과가 `Accepted`일 때 티켓을 첨부한다.

```bash
APP="src-tauri/target/release/bundle/macos/Directory UI Viewer.app"
VERSION=$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")
DMG="src-tauri/target/release/bundle/dmg/Directory UI Viewer_${VERSION}_aarch64.dmg"

codesign --verify --deep --strict --verbose=2 "$APP"
spctl --assess --type execute --verbose=2 "$APP"
xcrun stapler validate "$APP"
codesign --force --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$DMG"
xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
```

```bash
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
hdiutil verify "$DMG"
shasum -a 256 "$DMG"
```

4. 다른 Apple Silicon Mac에서 내려받은 DMG를 열고 앱을 응용 프로그램으로 복사한다. 기본 보안 설정에서 실행한 뒤 테스트 폴더 선택·취소, 보기 전환, 파일 보기, 새로고침을 확인한다.
5. 검증한 커밋으로 GitHub Pre-release를 만들고 최종 DMG, SHA-256, 설치 방법과 검증한 OS 버전을 올린다. README의 설치 상태와 링크를 같은 시점에 갱신한다.
