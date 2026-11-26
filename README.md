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

## Робота

Створіть профіль, оберіть `.bin` і теку для емуляції SD(рекомендується обирати одразу як теку в профілі). Дані створюються та зберігаються в стандартному користувацькому каталозі `lilka-emulator`; `LILKA_DATA_DIR` перевизначає його. Зазвичай створює в `.local/share/lilka-emulator`  Файл `portable.txt` поряд із програмою вмикає локальну теку `data`.

Стрілки, Z/X/C/V, Tab, Enter; P — пауза, F5 — перезапуск. Wi-Fi — відкрита віртуальна мережа через інтернет хоста, без WPA. Повернення до профілів замінює поточний процес, щоб заново ініціалізувати QEMU.

## Що є, чого нема, що буде, а шо точно НЕ буде
### Зараз емулятор здатний:
 - Працювати з потенційно всіма .bin софтом.
 - Емуляція екрану, кнопок керування, аудіо I2S та буззера.
 - **ЕКСПЕРИМЕНТАЛЬНА** підтримка Wi-Fi, може використовуватись наприклад для встановлення додатків з каталога.
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
