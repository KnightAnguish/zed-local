Zed Local 1.27.0
Локальная сборка Zed для Windows x64
=====================================

Состав
------
- zedlocal.exe  - локальная сборка Zed
- cli.exe       - CLI
- conpty.dll, OpenConsole.exe - терминал
- amd_ags_x64.dll - AMD GPU Services
- agents\codex-acp - локальный ACP-мост Codex
- Install.exe   - нативный установщик (без PowerShell)
- ZedLocalLauncher.exe - нативный лаунчер текущей сборки
- Uninstall.cmd - удаление (пока PowerShell-обёртка)
- README.txt

Установка
---------
1. Распакуйте ZIP в любую папку.
2. Запустите Install.exe. Он скопирует сборку в профиль пользователя и создаст ярлыки.
3. Запускайте Zed Local с ярлыка на рабочем столе или из меню «Пуск».

Программа ставится в:
  %LOCALAPPDATA%\Programs\ZedLocal\builds\<версия>

Данные (история, настройки, чаты) хранятся отдельно:
  %LOCALAPPDATA%\ZedLocal\UserData

Ярлык запускает ZedLocalLauncher.exe. Лаунчер читает install.json и открывает
текущую сборку. Никаких VBS/PowerShell в цепочке запуска нет.

Автообновление
--------------
Обновление встроено в Zed. При запуске и раз в час он проверяет последний
релиз форка. Если версия новее, в title bar появляется кнопка
"ZedLocal ... available"; ничего не скачивается без нажатия.

По нажатию Zed скачивает ZIP, проверяет SHA256, распаковывает и ставит новую
версию нативно, без PowerShell. Текущая сессия продолжает работать на старой
версии; новая запустится после закрытия и следующего запуска через ярлык.
Предыдущая версия сохраняется для отката, более старые удаляются.

Портативный режим
-----------------
Распакуйте ZIP и запустите zedlocal.exe напрямую с --user-data-dir <папка>.
Автообновление в portable-режиме не используется.

Удаление
--------
Запустите ZedLocalLauncher.exe --uninstall из папки установки.
Ярлыки и файлы программы удаляются; папка %LOCALAPPDATA%\ZedLocal\UserData
сохраняется.

Внешние агенты
--------------
- OpenCode устанавливается отдельно (registry/offline addon) и требует
  собственной авторизации.
- codex-acp уже лежит в agents\codex-acp. Укажите путь к index.js в settings.json
  и CODEX_PATH в окружении. Пример есть в agents\codex-acp\README.txt.

Требования
----------
- Windows 10/11 x64.
- Microsoft Visual C++ Redistributable 2015-2022 (x64).
- Microsoft Edge WebView2 Runtime - для HTML-предпросмотра.
- Node.js - только если используется codex-acp.

Проверка целостности
--------------------
SHA256SUMS.txt содержит хеши файлов программы. Install.exe проверяет manifest.json
(SHA256 каждого файла сборки), а автообновление проверяет SHA256 всего ZIP.

Лицензии
--------
Zed распространяется по GPL-3.0-or-later и Apache-2.0; файлы LICENSE-GPL и
LICENSE-APACHE приложены. Исходники и локальные коммиты:
https://github.com/KnightAnguish/zed-local
