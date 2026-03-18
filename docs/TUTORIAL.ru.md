# Подключение Codex Companion к Zed

Этот репозиторий уже содержит готовый Zed extension и companion MCP server. На 17 марта 2026 Zed умеет запускать Codex напрямую, поэтому здесь мы не дублируем сам Codex, а усиливаем его через локальный MCP слой: память, кэш индекса проекта, быстрый context bundle и slash-команды.

## Что получится

После установки у тебя будет связка:

- встроенный Codex thread в Zed
- дополнительный context server `codex-companion`
- slash-команды `/codex-context`, `/codex-memory`, `/codex-cache`, `/codex-refresh`, `/codex-warm`, `/codex-plan`, `/codex-orchestrate`
- slash-команда `/codex-skills` для поиска внешних skill/agent packs
- MCP tools `cache_status`, `build_context_bundle`, `orchestrate_task`, `decompose_task`, `search_skills` для Codex ACP thread, где `/codex-*` может перехватывать сам host
- persistent memory между сессиями
- локальный кэш проекта в пользовательской cache-директории OS
- фоновый prewarm кэша, git summary и memory store
- декомпозиция задач на workstreams с подсказками по параллелизации
- подключаемые внешние скиллы из настраиваемых директорий, например `C:\path\to\agency-agents`

## Шаг 1. Собери локальный сервер

Открой терминал в корне репозитория и выполни:

```powershell
cargo build --release -p codex-companion-server
```

После этого появится бинарь:

- Windows: `target\release\codex-companion-server.exe`
- macOS/Linux: `target/release/codex-companion-server`

Это основной companion-сервер, который Zed extension будет запускать как MCP server.

## Шаг 2. Установи репозиторий как Dev Extension

В Zed:

1. Открой Command Palette.
2. Выполни `zed: extensions`.
3. Нажми `Install Dev Extension`.
4. Укажи папку этого репозитория.

Если Rust установлен через `rustup`, Zed соберет wasm-часть расширения.

## Шаг 3. Включи context server

Дальше открой:

1. `Agent Panel`
2. `Settings`
3. Найди `codex-companion`
4. Включи сервер

Если Zed покажет installation instructions, это нормально: они просто напоминают, что локальный сервер должен быть заранее собран.

Важно: `default_settings.jsonc`, который Zed показывает для `codex-companion`, это справочный preview с дефолтами, а не живой файл настроек. Реальные изменения вноси через `Agent Panel -> Settings`, команду `agent: open settings` или через `.zed/settings.jsonc` в workspace.

Если после включения ты видишь `Waiting for context service` дольше 10-20 секунд, почти всегда это значит, что Zed не нашел бинарь companion-сервера автоматически. В таком случае открой настройки `codex-companion` и укажи явный путь:

```json
{
  "context_servers": {
    "codex-companion": {
      "settings": {
        "server_path": "D:\\downloads\\zed-codex\\target\\release\\codex-companion-server.exe"
      }
    }
  }
}
```

После этого выключи и снова включи `codex-companion`, либо перезапусти Zed.

## Шаг 4. Подключи сервер к профилю Codex

Самый удобный вариант:

1. В `Agent Panel -> Settings` создай отдельный профиль, например `Codex + Companion`
2. Для этого профиля оставь включенным `codex-companion`
3. Используй именно этот профиль для новых Codex threads

Важно: создание, хранение и удаление старых Codex threads контролирует сам Zed. `codex-companion` не может автоматически удалять старые треды при создании нового. Для ручной очистки используй встроенные действия Zed: `agent: remove selected thread` или `agent: remove history`.

Пример профиля, если захочешь редактировать JSON вручную:

```json
{
  "agent": {
    "profiles": {
      "codex-companion": {
        "name": "Codex + Companion",
        "enable_all_context_servers": false,
        "context_servers": {
          "codex-companion": {}
        }
      }
    }
  }
}
```

## Шаг 5. Запусти первый Codex thread

В Zed:

1. Открой `Agent Panel`
2. Нажми `+`
3. Создай `Codex` thread
4. Выбери профиль `Codex + Companion`, если ты его создал

После этого Codex сможет использовать инструменты companion-сервера:

- `warm_workspace`
- `workspace_overview`
- `search_workspace`
- `build_context_bundle`
- `orchestrate_task`
- `decompose_task`
- `remember_memory`
- `recall_memory`
- `recent_changes`

## Шаг 6. Используй slash-команды

В сообщении агенту можно вызывать:

- `/codex-context <задача>`
  - собирает готовый context bundle по текущему workspace
- `/codex-memory [запрос]`
  - вытаскивает сохраненную память по проекту
- `/codex-cache`
  - показывает состояние кэша и индекса
- `/codex-refresh`
  - принудительно пересобирает индекс
- `/codex-warm`
  - заранее прогревает индекс, git summary и memory store
- `/codex-plan <задача>`
  - разбивает крупную задачу на workstreams, coordination notes и parallelization hints
- `/codex-orchestrate <задача>`
  - запускает полный orchestration pipeline: skills, context bundle, decomposition и subagent-ready briefs
- `/codex-skills [запрос]`
  - ищет подходящие внешние skills/agent profiles в подключенных skill roots

Важно: в Codex ACP thread slash-команды расширения могут быть недоступны, потому что `/...` там парсит сам `codex-acp`. В этом режиме используй MCP tools напрямую:

- `cache_status`
- `build_context_bundle`
- `orchestrate_task`
- `decompose_task`
- `search_skills`

Практичный стартовый паттерн:

```text
/codex-context нужно добавить в плагин настройку для multi-root workspace
```

или

```text
/codex-memory auth flow
```

или

```text
/codex-plan нужно ускорить индексатор и вынести docs-правки в отдельный поток работы
```

или

```text
/codex-orchestrate нужно разбить модуль на subagents и подобрать skills по каждому workstream
```

или

```text
/codex-skills rapid prototype
```

## Шаг 7. Подключи внешние skills

Если хочешь, чтобы Codex Companion брал skills из `C:\path\to\agency-agents`, добавь в настройки сервера:

```json
{
  "context_servers": {
    "codex-companion": {
      "settings": {
        "skill_roots": [
          "C:\\path\\to\\agency-agents"
        ],
        "skill_file_globs": [
          "**/*.md"
        ],
        "max_skill_bytes": 131072,
        "skill_cache_ttl_secs": 300,
        "max_skills_per_query": 6
      }
    }
  }
}
```

Важно: `agency-agents` хранит роли в обычных `.md`-файлах, и companion теперь умеет индексировать такой формат. Не нужен отдельный `SKILL.md`.

## Шаг 8. Включи auto-approve там, где это уместно

Если тебе нужен максимально быстрый workflow в trusted workspace, у Zed есть официальная настройка `agent.tool_permissions.default`. Значение `"allow"` автоматически подтверждает tool actions.

Пример:

```json
{
  "agent": {
    "tool_permissions": {
      "default": "allow",
      "tools": {
        "mcp:codex-companion:warm_workspace": {
          "default": "allow"
        },
        "mcp:codex-companion:decompose_task": {
          "default": "allow"
        }
      }
    }
  }
}
```

Важно: это ускоряет подтверждение инструментов, но не заставляет сам Codex ACP magically получить sandbox bypass. Настоящий `full access`, shell approvals и поддержка subagents зависят от самого Codex host/ACP адаптера.

## Настройки сервера

У `codex-companion` есть собственные настройки. Самые полезные:

- `cache_dir`
  - куда хранить persistent memory и индекс
- `server_path`
  - абсолютный путь к бинарю `codex-companion-server`; самый полезный fallback для dev install
- `release_repo`
  - GitHub repo с готовыми релизными архивами сервера
- `ignore_globs`
  - дополнительные исключения из индекса
- `max_file_bytes`
  - максимальный размер файла для индексации
- `max_indexed_files`
  - общий лимит файлов в кэше
- `enable_git_tools`
  - включать ли git-aware tools
- `refresh_window_secs`
  - как долго держать in-memory индекс свежим без повторного сканирования
- `git_cache_ttl_secs`
  - как долго переиспользовать git summary без нового вызова `git`
- `bundle_cache_ttl_secs`
  - TTL для task-focused context bundle и decomposition cache
- `prewarm_on_start`
  - прогревать ли кэш, git и memory автоматически сразу после старта сервера
- `execution_mode`
  - режим подсказок для Codex: `careful`, `balanced` или `autonomous`
- `prefer_full_access`
  - advisory hint: просить Codex предпочесть full-access/auto-approved режим, если сам host это умеет
- `max_parallel_workstreams`
  - сколько независимых workstreams предлагать при декомпозиции задачи
- `skill_roots`
  - список директорий, из которых брать внешние skills/agent packs
- `skill_file_globs`
  - какие markdown-файлы внутри этих директорий считать skill-документами
- `max_skill_bytes`
  - максимальный размер skill-файла для индексации
- `skill_cache_ttl_secs`
  - TTL in-memory каталога внешних skills
- `max_skills_per_query`
  - сколько skill matches возвращать в bundle/decomposition/search

Если тебе удобнее настраивать через UI, просто открой настройки сервера в Agent Panel. Если через JSON, используй структуру, которую Zed сгенерирует для extension context server.

Не редактируй `configuration/default_settings.jsonc` как будто это пользовательский settings-файл: этот текст вшит в расширение и нужен Zed как preview дефолтов.

## Где лежат кэши

По умолчанию companion использует системную cache-директорию:

- Windows: `%LOCALAPPDATA%\codex\codex-companion\cache`
- macOS: `~/Library/Caches/dev.codex.codex-companion` или системный аналог, который вернет `directories`
- Linux: `~/.cache/codex-companion` через системный cache path провайдера

Внутри создается отдельная папка на каждый workspace.

## Если захочешь распространять плагин дальше

В репозитории уже есть GitHub Actions:

- `ci.yml` для проверки сборки
- `release-server.yml` для сборки архивов companion-сервера

Чтобы extension мог скачивать готовые бинарники автоматически:

1. Запушь этот репозиторий на GitHub
2. Опубликуй релиз с архивами сервера
3. Укажи `release_repo` в настройках `codex-companion`

Тогда локальный `cargo build` пользователю больше не понадобится.

## Проверенный локально сценарий

На этой машине я проверил:

- `cargo check` для всего workspace
- `cargo test -p codex-companion-server`
- `cargo build --release -p codex-companion-server`
- запуск CLI сервера на текущем репозитории

То есть базовый путь установки через локальную сборку у тебя уже реально рабочий.

## Что companion умеет и чего не умеет

Companion умеет:

- кэшировать индекс проекта, memory store, git summary и task bundles
- кэшировать и искать внешние skills из настраиваемых директорий
- прогревать эти кэши заранее
- строить декомпозицию задач и подсказывать, где есть потенциал для parallel work
- подталкивать Codex к более автономному стилю работы через prompt/promptable context

Companion не умеет:

- сам включать sandbox bypass или shell `full access`
- сам спавнить ACP subagents внутри Codex host
- обходить approval model, которую выставляет Zed или внешний Codex adapter
- управлять списком нативных Codex threads в Zed или автоматически удалять старые треды

То есть companion усиливает Codex, но не заменяет permission model и orchestration model самого host.
