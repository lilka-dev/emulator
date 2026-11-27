# Lilka Emulator
<p align="center">
  <img src="assets/lilka.svg" alt="Lilka Emulator" width="120">
</p>

Емулятор пристрою Lilka для тестування або просто запуску програм та бінарників що написані для неї. Перекомпіляція взагалі не потрібна, можна запускати майже все, не прив'язано ніяк до якого небудь фреймворка по типу Arduino, LilkaOxide, Pixeler і т.д.

Працює через QEMU, але з власними патчами, та фронт-енд мостом написаний на Rust, QEMU прямо зашитий в бінарник, тому не дивуйтесь якщо вихідний executable може важити в районі 40 мб.

## Збірка — Linux x86-64

### Переносний пакет

GitHub Actions збирає пакет на Ubuntu 22.04 з базовою glibc 2.35 і набором інструкцій x86-64-v1. Потрібні бібліотеки додаються в `lib/`; glibc і графічні драйвери залишаються системними. Потрібен Linux x86-64 з glibc 2.35+ та сеанс X11 або Wayland.

Артефакт GitHub містить `lilka-emulator-linux-x86_64.tar.gz`. Розпакуйте саме цей архів: він зберігає право на виконання програми.

```sh
tar -xzf lilka-emulator-linux-x86_64.tar.gz
./lilka-emulator/lilka-emulator
```

Копіювати потрібно весь каталог. Прошивки та SD користувач додає сам. Список бібліотек і мінімальна glibc записуються в `runtime.json`, ліцензії — у `LICENSES/`.

Такий самий пакет можна зібрати через Docker BuildKit з будь-якого Linux-дистрибутива:

```sh
docker build --file packaging/Dockerfile --output type=local,dest=dist .
./dist/lilka-emulator/lilka-emulator
```

`--build-arg JOBS=2` обмежує паралельність у контейнері. Каталог `dist/` має належати користувачу, який запускає збірку.

### Локальна збірка

Потрібні Python 3.9+ з актуальними виправленнями безпеки, Rust stable через rustup, C/C++, Make, Ninja, `patch`, `pkg-config` і заголовки GLib, Pixman та libgcrypt. Python-оточення QEMU створюється автоматично; Meson і pycotap постачаються в його зафіксованих вихідниках. Перша збірка потребує інтернету.

Ubuntu / Debian:

```sh
sudo apt update
sudo apt install --no-install-recommends \
  build-essential binutils git patch curl ca-certificates ninja-build pkg-config \
  python3 python3-venv python3-pip libglib2.0-dev libpixman-1-dev libgcrypt20-dev
```

openSUSE Tumbleweed:

```sh
sudo zypper install gcc gcc-c++ make binutils git patch curl ca-certificates \
  ninja pkgconf-pkg-config python3 python3-pip glib2-devel libpixman-1-0-devel libgcrypt-devel
```

Для Python старіше 3.11 також потрібен `python3-tomli` (у Ubuntu 22.04: `sudo apt install python3-tomli`).

Заголовки ALSA, udev, X11, Wayland, libfdt, GTK, SDL, RDMA та Python для цієї збірки не потрібні. Бібліотеки графічного сеансу й аудіо завантажуються під час роботи. Мінімальний контейнер без робочого столу потребує їхніх runtime-пакетів; повний перелік для пакування є в `packaging/Dockerfile`.

Якщо Rust ще не встановлений:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
```

У корені проєкту:

```sh
python3 build.py
./dist/lilka-emulator/lilka-emulator
```

Скрипт перевіряє SHA-256 вихідників, застосовує патчі, збирає лише плату ESP32-S3 та компонує QEMU з Rust-інтерфейсом. Окремий системний QEMU не потрібен. Локальна збірка використовує glibc хоста; для поширення між дистрибутивами використовуйте контейнерний пакет вище.

Кеші QEMU зберігаються в `.build/`, Rust — у `target/` або `CARGO_TARGET_DIR`. Зміна патчів автоматично створює новий кеш вихідників. Повторна команда використовує готові об'єкти. `--jobs` обмежує паралельність і Rust, і QEMU; `--no-lto` пришвидшує компонування під час розробки, ціною можливої втрати продуктивності емуляції:

```sh
python3 build.py --jobs 2 --no-lto
python3 build.py --help
```

`--portable` додає бібліотеки до локального пакета; для нього потрібні `patchelf`, `ldconfig`, runtime-бібліотеки з Dockerfile та їхні встановлені ліцензії. Цей параметр сам по собі не знижує вимогу до glibc хоста.

### Перевірка

```sh
LILKA_DATA_DIR=/tmp/lilka-tests cargo test --workspace --lib --locked
./dist/lilka-emulator/lilka-emulator --cpu-only --version
```

Тестові профілі зберігаються окремо від користувацьких. Ліцензії та вимоги до поширення наведено нижче.

## Збірка — macOS (Apple Silicon)

Потрібні Xcode Command Line Tools, Homebrew, Rust stable через rustup і Python 3.11+ з Homebrew: системний Python 3.9 не має `tarfile.data_filter`.

```sh
xcode-select --install
brew install python@3.11 pkgconf ninja glib pixman libgcrypt
python3.11 build.py
./dist/lilka-emulator/lilka-emulator
```

Збирається той самий QEMU, плюс патч `qemu/patches/qemu-darwin-host.patch`: фронтенд компонується з фреймворками AppKit, Foundation, CoreGraphics та іншими замість Linux-бібліотек, а символи `lilka_cpu_*` залишаються експортованими через `-export_dynamic`. Після `strip` бінарник підписується ad-hoc, бо Apple Silicon не запускає непідписаний код. Міст `liblilka_qemu_board_bridge.so` на macOS — це dylib під тим самим ім'ям. Дані профілів зберігаються в `~/Library/Application Support/lilka-emulator`. `--portable` доступний лише на Linux.

## Збірка — браузер (WebAssembly)

Той самий QEMU з платою Lilka збирається Emscripten у WebAssembly й працює на сторінці без встановлення. Потрібні [Emscripten](https://emscripten.org/docs/getting_started/downloads.html) (`emcc` у `PATH` або змінна `EMSDK`; без них скрипт шукає `.build/emsdk`), Rust stable з ціллю й вихідниками std, Python 3.10+, Ninja, `patch`, `pkg-config`, autotools-збірка (`make`) та Node (входить до emsdk).

```sh
git clone https://github.com/emscripten-core/emsdk.git .build/emsdk
EMSDK_PYTHON=python3.11 .build/emsdk/emsdk install latest
EMSDK_PYTHON=python3.11 .build/emsdk/emsdk activate latest
rustup target add wasm32-unknown-emscripten
rustup component add rust-src
python3.11 web/build.py
python3.11 web/serve.py   # http://localhost:8080
```

Перша збірка кросс-компілює zlib, libffi, pixman, GLib, libgpg-error і libgcrypt у `.build/web/sysroot` (перевірені SHA-256, потрібен інтернет), далі готує вихідники QEMU так само, як нативна збірка, і додає патчі з `qemu/web-manifest.json`:

- `qemu/patches/web/qemu-emscripten-host.patch` — підтримка Emscripten із QEMU 10.1 (автор Kohei Tokunaga), перенесена на 9.2: TCI-інтерпретатор замість JIT, coroutine через fiber-и Emscripten, виправлення типів вказівників на функції (WebAssembly перевіряє сигнатуру кожного непрямого виклику). Плюс прискорення для браузера: прямі виклики TCG-хелперів замість libffi, швидкий шлях TLB у TCI (як у нативних бекендах), коротке очікування на м'ютексах перед сном (пробудження потоку в wasm дороге), `setjmp`/`longjmp` на винятках WebAssembly замість JavaScript і MMIO посеред блоку без перекомпіляції, коли icount вимкнений (`LILKA_QEMU_STRICT_IO=1` повертає точну поведінку).
- `qemu/patches/web/qemu-lilka-web.patch` — міст плати компонується статично (без `dlopen`), а `main` передає керування `src/web.rs`.

Rust-частина збирається як одна статична бібліотека: `cargo build -p lilka-emulator --target wasm32-unknown-emscripten --no-default-features --features web` з `-Zbuild-std` (std перезбирається з атомарними операціями для спільної пам'яті потоків). Результат — `dist/web`: `qemu-system-xtensa.js/.wasm`, сторінка, ROM та ліцензії.

Сторінка потребує `SharedArrayBuffer`, тому сервер має надсилати заголовки `Cross-Origin-Opener-Policy: same-origin` і `Cross-Origin-Embedder-Policy: require-corp` (`web/serve.py` так робить). Для хостингу без власних заголовків (наприклад, GitHub Pages) їх додає `coi-serviceworker.js`; потрібен HTTPS або localhost, а перше відкриття сторінки один раз перезавантажує її.

Сайт статичний: workflow `Web` збирає `dist/web`, перевіряє запуск KeiraOS і після пушу в основну гілку публікує його на GitHub Pages. Один раз увімкніть це в налаштуваннях репозиторію: Settings → Pages → Source: GitHub Actions.

Кнопка «Завантажити останню KeiraOS» бере останній реліз [lilka-dev/keira](https://github.com/lilka-dev/keira/releases) українською або англійською. Workflow кладе його поруч зі сторінкою (`firmware/latest.json`, `firmware/<тег>/KeiraOS_merged_LANG_*.bin`, як flasher.lilka.dev) і перед публікацією перевіряє, що саме цей реліз запускається. Локальна збірка без цих файлів бере копію з flasher.lilka.dev. Новий реліз Keira перевидає сайт через подію `repository_dispatch` типу `keira-release` (ту саму, яку чекає flasher): у workflow релізу Keira після завантаження файлів потрібен крок

```yaml
      - name: Notify flasher and emulator
        if: startsWith(github.ref, 'refs/tags/')
        env:
          GH_TOKEN: ${{ secrets.LILKA_DISPATCH_TOKEN }}
        run: |
          for repo in lilka-dev/flasher lilka-dev/emulator; do
            gh api "repos/$repo/dispatches" -f event_type=keira-release \
              -f "client_payload[tag]=${{ steps.version.outputs.TAG }}"
          done
```

і секрет `LILKA_DISPATCH_TOKEN` з правом запису Contents в обох репозиторіях.

У браузері прошивка, стан флеш-пам'яті та файли SD зберігаються в IndexedDB цього сайту. Зміни, які програма записала на SD, потрапляють у сховище під час «Зупинити й зберегти»; там же можна завантажити SD як ZIP. Сторінка перезавантажується лише тоді, коли SD записано й збережено в IndexedDB; якщо запис триває понад 15 с, вона чекає далі й дозволяє перезавантажитися без збереження, а помилку показує замість перезавантаження. Wi-Fi вимкнений: браузер не дає програмі сирих TCP/UDP-сокетів.

Процесор інтерпретується (TCI), але з двома потоками (по одному на ядро ESP32-S3) KeiraOS показує перше зображення приблизно за 3–4 с і готове меню за 5 с (нативно — 2,5 с і 4 с). Ядра емулюються окремими потоками, що часто чекають одне на одного, тож запуск залежить від вільних ядер комп'ютера: коли інша програма чи ще одна вкладка з емулятором займає 2–3 ядра, перше зображення з'являється за 5–6 с. Мелодію запуску KeiraOS пише в I2S по одному семплу, і інтерпретований процесор ледве встигає: QEMU тоді притримує час гостя, тож звук надходить на 93–98% від 48 кГц, а потім надолужує до 102%. Програвач (`web/static/audio-worklet.js`) вимірює цей темп і відтворює з ним (у межах 90–105%), тож мелодія звучить без розривів і переповнень, лише трохи нижче. Перевірки без браузера:

```sh
node web/test/boot.mjs firmware/KeiraOS_merged_LANG_UK.bin 40 --frame /tmp/keira.ppm --audio-trace /tmp/audio.csv
node web/test/audio.mjs /tmp/audio.csv
```

`boot.mjs` проходить, лише якщо екран засвітився і емулятор наприкінці ще працює; `audio.mjs` перевіряє програвач на повільному, швидкому, нерівному й записаному потоці звуку.

## Робота

Створіть профіль, оберіть `.bin` і теку для емуляції SD(рекомендується обирати одразу як теку в профілі). Дані створюються та зберігаються в стандартному користувацькому каталозі `lilka-emulator`; `LILKA_DATA_DIR` перевизначає його. Зазвичай створює в `.local/share/lilka-emulator`  Файл `portable.txt` поряд із програмою вмикає локальну теку `data`.

Стрілки, Z/X/C/V, Tab, Enter; P — пауза, F5 — перезапуск. Wi-Fi — відкрита віртуальна мережа через інтернет хоста, без WPA. Повернення до профілів замінює поточний процес, щоб заново ініціалізувати QEMU.

## Що є, чого нема, що буде, а шо точно НЕ буде
### Зараз емулятор здатний:
 - Працювати з потенційно всіма .bin софтом.
 - Емуляція екрану, кнопок керування, аудіо I2S та буззера.
 - **ЕКСПЕРИМЕНТАЛЬНА** підтримка Wi-Fi, може використовуватись наприклад для встановлення додатків з каталога.
 - Працювати в браузері (WebAssembly): повільніше за нативну збірку й без Wi-Fi.
### Чого немає але планується(типу Roadmap)
 - UART емуляція для логів, і для Lua Live(частково все це є, треба тільки пронести в інтерфейс)
 - Не вся внутрішня емуляція точна
 - Bluetooth(можливо його і не буде взагалі)
 - Керування через gamepad, або через Lilka
 - Підтримка Windows; готовий пакет для macOS (поки лише локальна збірка на Apple Silicon)
### Що точно не буде
 - Емуляція фізичних провідних інтерфейсів по типу I2C, SPI і т.д. (т.ч. GPIO)
 - Емуляція інших інтерфейсів які з великою вірогідністю не будуть працювати, по типу USB чи Bluetooth

## Я хочу допомогти писати код, або якось ще вплинути!
 - Завжди відкриті до Pull Request для нових фіч та виправлення багів.
 - Потребуємо завжди Issues коли виникли баги та якісь інциденти з прошивками, програмами.
 - Також можете писати там же ідеї покращення любого аспекту, або в Discord спільноті.
 
## Використаний код і ліцензії

- [Espressif QEMU](https://github.com/espressif/qemu), `esp-develop-9.2.2-20260417`: GPLv2; окремі файли мають власні сумісні ліцензії. TCG виконує код Xtensa. Локальні зміни й контрольні суми — `qemu/`.
- Модель Wi-Fi використовує [mikroverk/esp32sim](https://github.com/mikroverk/esp32sim), commit `d043ff1dd3e53aca18a23f4f2b18bfb038a57667`: MIT.


Тексти основних ліцензій — `LICENSES/`. При поширенні модифікованого QEMU потрібні відповідні вихідники, патчі та інструкції збірки. Користувацькі прошивки й ігрові ресурси до пакета не входять. ROM ESP32-S3 надається Espressif і має окремі умови постачальника.
