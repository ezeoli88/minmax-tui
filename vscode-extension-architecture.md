# Arquitectura: Extensión VSCode para MinMax

## Resumen

Este documento describe la arquitectura para crear una extensión de VSCode que exponga la funcionalidad de MinMax (asistente de código con IA) directamente dentro del editor, reemplazando la interfaz TUI basada en Ink por la API nativa de VSCode (Webview, TreeView, OutputChannel, etc.).

---

## 1. Qué se puede reutilizar del proyecto actual

El proyecto `minmax-tui` tiene una separación de capas que facilita la extracción:

### Capa reutilizable directamente (core/)

| Módulo | Archivo | Qué hace | Adaptaciones necesarias |
|--------|---------|----------|------------------------|
| **API Client** | `core/api.ts` | Cliente streaming contra MiniMax API | Ninguna — funciona tal cual con `openai` SDK |
| **Parser** | `core/parser.ts` | Parseo de XML (`<think>`, `<minimax:tool_call>`) | Ninguna — es puro TypeScript sin dependencias de UI |
| **Tool Registry** | `core/tools.ts` | Registro y dispatch de herramientas | Menor — cambiar imports a la nueva ubicación |
| **Tool implementations** | `tools/*.ts` | bash, read_file, write_file, edit_file, glob, grep, list_dir | Revisar — `bun:sqlite` no existe en Node, pero las tools usan solo fs/child_process |
| **MCP Client** | `core/mcp.ts` | Conexión a servidores MCP via stdio | Ninguna — `@modelcontextprotocol/sdk` funciona en Node |
| **Session DB** | `core/session.ts` | Persistencia SQLite | **Cambiar** — reemplazar `bun:sqlite` por `better-sqlite3` o usar `vscode.Memento`/archivos JSON |
| **Settings** | `config/settings.ts` | Config load/save | **Reemplazar** — usar `vscode.workspace.getConfiguration()` |

### Capa NO reutilizable (reescribir para VSCode)

| Módulo | Por qué | Reemplazo en VSCode |
|--------|---------|---------------------|
| `app.tsx`, `ChatInterface.tsx` | React/Ink (terminal UI) | Webview Panel con HTML/CSS/JS |
| `MessageList.tsx` | Renderizado virtualizado para terminal | HTML con scroll nativo o virtualización web |
| `Input.tsx` | Input de terminal (Ink) | `<textarea>` en Webview o `vscode.window.createInputBox()` |
| `Header.tsx`, `StatusBar.tsx` | Componentes Ink | Barra de estado nativa de VSCode + Webview header |
| `CommandPalette.tsx` | Paleta de comandos Ink | Comandos registrados en VSCode (`vscode.commands`) |
| `SessionPicker.tsx` | Selector de sesiones Ink | QuickPick nativo de VSCode |
| `ConfigMenu.tsx` | Menú de config Ink | Settings UI nativa de VSCode |
| `hooks/useChat.ts` | React hook con useState/useRef | **Clase TypeScript** con EventEmitter o similar |
| `hooks/useMode.ts` | React hook | Estado simple en la clase del chat |
| `hooks/useSession.ts` | React hook | Integrado en SessionManager |
| `index.ts` | Bootstrap con Ink render() | `extension.ts` con `activate()/deactivate()` |

---

## 2. Arquitectura propuesta

```
minmax-vscode/
├── package.json                    # Manifest de la extensión VSCode
├── tsconfig.json
├── src/
│   ├── extension.ts                # Punto de entrada: activate() / deactivate()
│   │
│   ├── core/                       # === REUTILIZADO de minmax-tui ===
│   │   ├── api.ts                  # Cliente MiniMax (sin cambios)
│   │   ├── parser.ts               # Parser XML (sin cambios)
│   │   ├── tools.ts                # Registry de tools (sin cambios)
│   │   └── mcp.ts                  # Cliente MCP (sin cambios)
│   │
│   ├── tools/                      # === REUTILIZADO de minmax-tui ===
│   │   ├── bash.ts                 # (adaptar: usar workspace root como cwd)
│   │   ├── read-file.ts
│   │   ├── write-file.ts
│   │   ├── edit-file.ts
│   │   ├── glob.ts
│   │   ├── grep.ts
│   │   └── list-dir.ts
│   │
│   ├── chat/                       # === NUEVO: lógica de chat sin React ===
│   │   ├── ChatEngine.ts           # Loop agéntico (extraído de useChat)
│   │   ├── SessionManager.ts       # Persistencia de sesiones
│   │   └── types.ts                # ChatMessage, Mode, etc.
│   │
│   ├── providers/                  # === NUEVO: integración VSCode ===
│   │   ├── ChatViewProvider.ts     # WebviewViewProvider (panel lateral)
│   │   ├── SessionTreeProvider.ts  # TreeDataProvider (árbol de sesiones)
│   │   └── StatusBarManager.ts     # Items de barra de estado
│   │
│   └── webview/                    # === NUEVO: UI del chat ===
│       ├── index.html              # Shell HTML del webview
│       ├── main.ts                 # Lógica del webview (postMessage)
│       ├── styles.css              # Estilos (soportar temas VSCode)
│       └── components/             # Componentes web (vanilla o Preact)
│           ├── MessageList.ts
│           ├── InputArea.ts
│           ├── ToolOutput.ts
│           └── ThinkingBlock.ts
│
├── media/                          # Iconos, assets
│   └── icon.png
│
└── test/
    └── ...
```

---

## 3. Componentes clave y sus responsabilidades

### 3.1 `extension.ts` — Punto de entrada

```typescript
import * as vscode from 'vscode';

export function activate(context: vscode.ExtensionContext) {
  // 1. Registrar el Webview Provider (panel lateral)
  const chatProvider = new ChatViewProvider(context);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('minmax.chatView', chatProvider)
  );

  // 2. Registrar el TreeView de sesiones
  const sessionTree = new SessionTreeProvider(context);
  vscode.window.createTreeView('minmax.sessions', { treeDataProvider: sessionTree });

  // 3. Registrar comandos
  context.subscriptions.push(
    vscode.commands.registerCommand('minmax.newSession', () => chatProvider.newSession()),
    vscode.commands.registerCommand('minmax.toggleMode', () => chatProvider.toggleMode()),
    vscode.commands.registerCommand('minmax.cancel', () => chatProvider.cancelStream()),
    vscode.commands.registerCommand('minmax.selectModel', () => showModelPicker()),
  );

  // 4. Barra de estado
  const statusBar = new StatusBarManager();
  context.subscriptions.push(statusBar);

  // 5. Inicializar MCP servers (si están configurados)
  initMCPServers(getConfig().mcpServers);
}

export function deactivate() {
  shutdownMCPServers();
}
```

### 3.2 `ChatEngine.ts` — Loop agéntico (corazón de la app)

Esto es la refactorización de `useChat.ts` sin dependencias de React:

```typescript
import { EventEmitter } from 'events';
import type OpenAI from 'openai';
import { streamChat, type AccumulatedToolCall } from '../core/api';
import { executeTool, getToolDefinitions, getReadOnlyToolDefinitions } from '../core/tools';
import { parseModelOutput, coerceArg } from '../core/parser';

type Mode = 'PLAN' | 'BUILDER';

interface ChatMessage {
  role: 'user' | 'assistant' | 'system' | 'tool';
  content: string;
  reasoning?: string;
  toolCalls?: AccumulatedToolCall[];
  toolCallId?: string;
  name?: string;
  isStreaming?: boolean;
}

// Eventos que el ChatEngine emite hacia la UI (Webview)
interface ChatEngineEvents {
  'message': (msg: ChatMessage) => void;
  'message:update': (index: number, msg: Partial<ChatMessage>) => void;
  'loading': (isLoading: boolean) => void;
  'tokens': (total: number) => void;
  'tool:start': (toolCallId: string, toolName: string) => void;
  'tool:done': (toolCallId: string, result: string) => void;
  'error': (error: string) => void;
}

export class ChatEngine extends EventEmitter {
  private client: OpenAI;
  private model: string;
  private mode: Mode = 'BUILDER';
  private messages: ChatMessage[] = [];
  private history: ChatCompletionMessageParam[] = [];
  private totalTokens = 0;
  private abortController: AbortController | null = null;
  private isLoading = false;

  constructor(client: OpenAI, model: string) {
    super();
    this.client = client;
    this.model = model;
  }

  async sendMessage(userInput: string): Promise<void> {
    if (this.isLoading) return;
    this.setLoading(true);

    // Agregar mensaje de usuario
    this.addMessage({ role: 'user', content: userInput });
    this.history.push({ role: 'user', content: userInput });

    try {
      let continueLoop = true;
      while (continueLoop) {
        continueLoop = false;
        this.abortController = new AbortController();

        // Placeholder streaming
        const streamIdx = this.addMessage({
          role: 'assistant', content: '', isStreaming: true
        });

        let rawBuffer = '';
        let structuredReasoning = '';

        const tools = this.mode === 'BUILDER'
          ? getToolDefinitions()
          : getReadOnlyToolDefinitions();

        const result = await streamChat(
          this.client, this.model,
          this.buildHistory(), tools,
          {
            onReasoningChunk: (chunk) => { /* emit update */ },
            onContentChunk: (chunk) => { /* emit update */ },
            onToolCallDelta: (tcs) => { /* emit update */ },
            onError: (err) => { /* emit error */ },
          },
          this.abortController.signal
        );

        // ... (misma lógica de useChat.ts: parse, merge tool calls, execute tools)
        // La diferencia: en lugar de setMessages(), emitir eventos
      }
    } finally {
      this.setLoading(false);
    }
  }

  toggleMode(): void { /* ... */ }
  cancelStream(): void { this.abortController?.abort(); }
  // ...
}
```

### 3.3 `ChatViewProvider.ts` — Panel Webview

```typescript
import * as vscode from 'vscode';
import { ChatEngine } from '../chat/ChatEngine';

export class ChatViewProvider implements vscode.WebviewViewProvider {
  private view?: vscode.WebviewView;
  private engine: ChatEngine;

  constructor(private context: vscode.ExtensionContext) {
    const apiKey = vscode.workspace.getConfiguration('minmax').get<string>('apiKey');
    const model = vscode.workspace.getConfiguration('minmax').get<string>('model');
    const client = createClient(apiKey);
    this.engine = new ChatEngine(client, model);

    // Reenviar eventos del engine al webview
    this.engine.on('message', (msg) => this.postToWebview({ type: 'newMessage', msg }));
    this.engine.on('message:update', (idx, patch) =>
      this.postToWebview({ type: 'updateMessage', idx, patch })
    );
    this.engine.on('loading', (v) => this.postToWebview({ type: 'loading', value: v }));
    this.engine.on('tokens', (t) => this.postToWebview({ type: 'tokens', total: t }));
  }

  resolveWebviewView(view: vscode.WebviewView) {
    this.view = view;
    view.webview.options = { enableScripts: true };
    view.webview.html = this.getHtml(view.webview);

    // Recibir mensajes del webview
    view.webview.onDidReceiveMessage((msg) => {
      switch (msg.type) {
        case 'sendMessage':
          this.engine.sendMessage(msg.text);
          break;
        case 'cancel':
          this.engine.cancelStream();
          break;
        case 'toggleMode':
          this.engine.toggleMode();
          break;
      }
    });
  }

  private postToWebview(msg: any) {
    this.view?.webview.postMessage(msg);
  }

  private getHtml(webview: vscode.Webview): string {
    // Retorna el HTML con CSP, links a main.js y styles.css
  }
}
```

### 3.4 Comunicación Extension Host <-> Webview

```
┌─────────────────────────────────────────────────────────┐
│  VSCode Extension Host (Node.js)                        │
│                                                         │
│  ┌──────────────┐     ┌──────────────┐                  │
│  │  ChatEngine   │────▶│ ChatView     │                  │
│  │  (agentic     │     │ Provider     │                  │
│  │   loop)       │     │              │                  │
│  │              │◀────│              │                  │
│  └──────┬───────┘     └──────┬───────┘                  │
│         │                    │                           │
│         │ executeTool()      │ postMessage()             │
│         ▼                    ▼                           │
│  ┌──────────────┐     ┌──────────────────────────────┐  │
│  │  Tools        │     │  Webview (iframe, sandboxed)  │  │
│  │  - bash       │     │                              │  │
│  │  - read_file  │     │  ┌────────────────────────┐  │  │
│  │  - write_file │     │  │  main.ts               │  │  │
│  │  - edit_file  │     │  │  - MessageList         │  │  │
│  │  - glob       │     │  │  - InputArea           │  │  │
│  │  - grep       │     │  │  - ToolOutput          │  │  │
│  │  - list_dir   │     │  │  - ThinkingBlock       │  │  │
│  │  - MCP tools  │     │  └────────────────────────┘  │  │
│  └──────────────┘     └──────────────────────────────┘  │
│                                                         │
│  ┌──────────────┐     ┌──────────────┐                  │
│  │  Session      │     │  StatusBar   │                  │
│  │  Manager      │     │  Manager     │                  │
│  └──────────────┘     └──────────────┘                  │
└─────────────────────────────────────────────────────────┘
```

La comunicación entre el Extension Host y el Webview es **siempre** via `postMessage()` (es un iframe sandboxed). Los mensajes se definen con un protocolo tipado:

```typescript
// Extension -> Webview
type ExtensionMessage =
  | { type: 'newMessage'; msg: ChatMessage }
  | { type: 'updateMessage'; idx: number; patch: Partial<ChatMessage> }
  | { type: 'loading'; value: boolean }
  | { type: 'tokens'; total: number }
  | { type: 'modeChanged'; mode: Mode }
  | { type: 'sessionLoaded'; messages: ChatMessage[] }
  | { type: 'quota'; info: QuotaInfo };

// Webview -> Extension
type WebviewMessage =
  | { type: 'sendMessage'; text: string }
  | { type: 'cancel' }
  | { type: 'toggleMode' }
  | { type: 'newSession' }
  | { type: 'loadSession'; sessionId: string }
  | { type: 'ready' };  // webview ha terminado de cargar
```

---

## 4. `package.json` de la extensión (manifest)

```jsonc
{
  "name": "minmax-vscode",
  "displayName": "MinMax AI Assistant",
  "description": "MiniMax-powered AI coding assistant for VSCode",
  "version": "0.1.0",
  "engines": { "vscode": "^1.85.0" },
  "categories": ["AI", "Chat"],
  "activationEvents": [],
  "main": "./dist/extension.js",

  "contributes": {
    // Panel lateral en la barra de actividad
    "viewsContainers": {
      "activitybar": [{
        "id": "minmax",
        "title": "MinMax",
        "icon": "media/icon.svg"
      }]
    },

    // Vistas dentro del contenedor
    "views": {
      "minmax": [
        { "type": "webview", "id": "minmax.chatView", "name": "Chat" },
        { "id": "minmax.sessions", "name": "Sessions" }
      ]
    },

    // Comandos
    "commands": [
      { "command": "minmax.newSession", "title": "MinMax: New Session" },
      { "command": "minmax.toggleMode", "title": "MinMax: Toggle Plan/Builder Mode" },
      { "command": "minmax.cancel", "title": "MinMax: Cancel Response" },
      { "command": "minmax.selectModel", "title": "MinMax: Select Model" },
      { "command": "minmax.openSettings", "title": "MinMax: Open Settings" }
    ],

    // Keybindings
    "keybindings": [
      { "command": "minmax.toggleMode", "key": "ctrl+shift+m", "mac": "cmd+shift+m" },
      { "command": "minmax.cancel", "key": "escape", "when": "minmax.isLoading" },
      { "command": "minmax.newSession", "key": "ctrl+shift+n", "mac": "cmd+shift+n" }
    ],

    // Configuración
    "configuration": {
      "title": "MinMax",
      "properties": {
        "minmax.apiKey": {
          "type": "string",
          "description": "MiniMax API Key",
          "scope": "application"
        },
        "minmax.model": {
          "type": "string",
          "default": "MiniMax-M2.5",
          "enum": ["MiniMax-M2.5", "MiniMax-M2.5-highspeed", "MiniMax-M2.1", "MiniMax-M2.1-highspeed"],
          "description": "Default model"
        },
        "minmax.mcpServers": {
          "type": "object",
          "default": {},
          "description": "MCP server configurations"
        },
        "minmax.autoScroll": {
          "type": "boolean",
          "default": true,
          "description": "Auto-scroll to bottom on new messages"
        }
      }
    }
  }
}
```

---

## 5. Consideraciones importantes

### 5.1 Runtime: Node.js vs Bun

La extensión corre en el Extension Host de VSCode que usa **Node.js**, no Bun. Esto implica:

| Aspecto | En minmax-tui (Bun) | En VSCode (Node.js) | Solución |
|---------|---------------------|---------------------|----------|
| SQLite | `bun:sqlite` (built-in) | No disponible | Usar `better-sqlite3` o archivos JSON en `globalStorageUri` |
| File I/O | `fs` (compatible) | `fs` (compatible) | Sin cambios |
| Subprocess | `Bun.spawn()` o `child_process` | `child_process` | Verificar que tools usen `child_process` |
| Fetch | Global `fetch` (Bun) | Global `fetch` (Node 18+) | Sin cambios (VSCode ≥1.85 usa Node ≥18) |

### 5.2 Seguridad del Webview

VSCode Webviews son iframes sandboxed. Se debe configurar Content Security Policy:

```html
<meta http-equiv="Content-Security-Policy" content="
  default-src 'none';
  style-src ${webview.cspSource} 'unsafe-inline';
  script-src 'nonce-${nonce}';
  font-src ${webview.cspSource};
">
```

Las tools (bash, write_file, etc.) se ejecutan en el Extension Host, **no** en el webview. Esto es correcto y seguro.

### 5.3 Almacenamiento de API Key

**Nunca** guardar la API key en `settings.json` (queda en texto plano y puede sincronizarse). Usar:

```typescript
// Guardar
await context.secrets.store('minmax.apiKey', apiKey);

// Leer
const apiKey = await context.secrets.get('minmax.apiKey');
```

`vscode.SecretStorage` usa el keychain del sistema operativo (Keychain en macOS, Credential Manager en Windows, libsecret en Linux).

### 5.4 Persistencia de sesiones

Opciones, de más simple a más robusta:

1. **Archivos JSON** en `context.globalStorageUri` — simple, sin dependencias
2. **`better-sqlite3`** — misma API que `bun:sqlite`, pero requiere native module bundling
3. **VSCode Memento** (`context.globalState`) — limitado a 256KB por key, no apto para historial largo

**Recomendación**: Empezar con archivos JSON (una carpeta `sessions/` con un JSON por sesión). Si el rendimiento es problema con muchas sesiones, migrar a SQLite.

### 5.5 Tema y estilos del Webview

VSCode expone variables CSS del tema activo. Usarlas en lugar de los temas custom de minmax-tui:

```css
body {
  background: var(--vscode-editor-background);
  color: var(--vscode-editor-foreground);
  font-family: var(--vscode-font-family);
  font-size: var(--vscode-font-size);
}

.message-user {
  background: var(--vscode-badge-background);
  color: var(--vscode-badge-foreground);
}

.message-assistant {
  background: var(--vscode-editorWidget-background);
  border: 1px solid var(--vscode-editorWidget-border);
}

.code-block {
  background: var(--vscode-textCodeBlock-background);
  font-family: var(--vscode-editor-font-family);
}

.tool-output {
  background: var(--vscode-terminal-background);
  color: var(--vscode-terminal-foreground);
}
```

Esto hace que la extensión se adapte automáticamente a cualquier tema (dark, light, high contrast).

### 5.6 Bundling

Usar **esbuild** (recomendado por VSCode) para bundlear la extensión:

```json
{
  "scripts": {
    "compile": "esbuild src/extension.ts --bundle --outfile=dist/extension.js --external:vscode --format=cjs --platform=node",
    "compile:webview": "esbuild src/webview/main.ts --bundle --outfile=dist/webview.js --format=iife",
    "package": "vsce package"
  }
}
```

Dos bundles separados:
- **extension.js**: Corre en Node.js (Extension Host)
- **webview.js**: Corre en el iframe del Webview (browser-like)

### 5.7 Integración con el editor

Funcionalidades que aprovechan VSCode más allá de un simple chat:

```typescript
// Insertar código directamente en el editor activo
vscode.commands.registerCommand('minmax.insertAtCursor', (code: string) => {
  const editor = vscode.window.activeTextEditor;
  if (editor) {
    editor.edit(edit => edit.insert(editor.selection.active, code));
  }
});

// Enviar selección actual como contexto
vscode.commands.registerCommand('minmax.explainSelection', () => {
  const editor = vscode.window.activeTextEditor;
  const selection = editor?.document.getText(editor.selection);
  if (selection) {
    chatEngine.sendMessage(`Explain this code:\n\`\`\`\n${selection}\n\`\`\``);
  }
});

// Menú contextual del editor
// En package.json contributes.menus:
"editor/context": [
  { "command": "minmax.explainSelection", "when": "editorHasSelection", "group": "minmax" },
  { "command": "minmax.fixSelection", "when": "editorHasSelection", "group": "minmax" }
]
```

### 5.8 Diffs para edit_file

En lugar de mostrar diffs en texto plano, usar la API nativa de VSCode:

```typescript
// Mostrar diff antes de aplicar un cambio
async function showDiff(filePath: string, newContent: string) {
  const uri = vscode.Uri.file(filePath);
  const tempUri = vscode.Uri.parse(`minmax-diff:${filePath}`);
  // Registrar un TextDocumentContentProvider para el contenido propuesto
  await vscode.commands.executeCommand('vscode.diff', uri, tempUri, 'MinMax: Proposed Changes');
}
```

---

## 6. Plan de implementación por fases

### Fase 1 — MVP funcional
- Scaffolding de la extensión con `yo code`
- Copiar `core/` y `tools/` del TUI (adaptar imports)
- Implementar `ChatEngine` (extraer lógica de `useChat.ts`)
- Webview básico con input + lista de mensajes
- Configuración de API key via SecretStorage
- Un solo comando: enviar mensaje

### Fase 2 — Paridad con el TUI
- Modo PLAN/BUILDER con toggle
- Sesiones: guardar, listar, restaurar (TreeView)
- Renderizado de markdown en mensajes
- Visualización de tool calls y resultados
- Bloque de "thinking" colapsable
- Barra de estado con tokens y modelo

### Fase 3 — Ventajas de VSCode
- Menú contextual: "Explain", "Fix", "Refactor" sobre selección
- Botón "Insertar en editor" en bloques de código
- Diff viewer nativo para cambios propuestos
- Integración con el panel de problemas (diagnostics)
- Soporte MCP servers configurables

### Fase 4 — Polish
- Onboarding (walkthrough de primer uso)
- Marketplace publishing
- Telemetría opt-in
- Tests E2E con `@vscode/test-electron`

---

## 7. Estructura de archivos final esperada

```
minmax-vscode/
├── .vscode/
│   ├── launch.json                 # Config para debug (Extension Host + Webview)
│   └── tasks.json                  # Build tasks
├── src/
│   ├── extension.ts                # activate() / deactivate()
│   ├── core/
│   │   ├── api.ts                  # ← copiado de minmax-tui (sin cambios)
│   │   ├── parser.ts               # ← copiado de minmax-tui (sin cambios)
│   │   ├── tools.ts                # ← copiado (ajustar imports)
│   │   └── mcp.ts                  # ← copiado (sin cambios)
│   ├── tools/
│   │   ├── bash.ts                 # ← copiado (usar workspace.rootPath como cwd)
│   │   ├── read-file.ts            # ← copiado
│   │   ├── write-file.ts           # ← copiado
│   │   ├── edit-file.ts            # ← copiado
│   │   ├── glob.ts                 # ← copiado
│   │   ├── grep.ts                 # ← copiado
│   │   └── list-dir.ts             # ← copiado
│   ├── chat/
│   │   ├── ChatEngine.ts           # Loop agéntico (refactor de useChat sin React)
│   │   ├── SessionManager.ts       # CRUD de sesiones (JSON files)
│   │   └── types.ts                # Interfaces compartidas
│   ├── providers/
│   │   ├── ChatViewProvider.ts     # WebviewViewProvider
│   │   ├── SessionTreeProvider.ts  # TreeDataProvider
│   │   ├── DiffContentProvider.ts  # TextDocumentContentProvider
│   │   └── StatusBarManager.ts     # StatusBarItem management
│   └── webview/
│       ├── index.html
│       ├── main.ts                 # Lógica principal del webview
│       ├── styles.css              # Estilos con CSS variables de VSCode
│       ├── markdown.ts             # Renderizado de markdown (usar marked o similar)
│       └── components/
│           ├── MessageList.ts
│           ├── InputArea.ts
│           ├── ToolOutput.ts
│           └── ThinkingBlock.ts
├── media/
│   ├── icon.svg
│   └── icon-dark.svg
├── test/
│   ├── suite/
│   │   ├── chatEngine.test.ts
│   │   └── extension.test.ts
│   └── runTest.ts
├── package.json
├── tsconfig.json
├── esbuild.mjs                     # Build script
├── .vscodeignore                   # Archivos a excluir del .vsix
└── README.md
```

---

## 8. Dependencias de la extensión

```json
{
  "dependencies": {
    "openai": "^4.77.0",
    "@modelcontextprotocol/sdk": "^1.3.0",
    "marked": "^12.0.0",
    "zod": "^3.24.0"
  },
  "devDependencies": {
    "@types/vscode": "^1.85.0",
    "@types/node": "^20.0.0",
    "esbuild": "^0.20.0",
    "typescript": "^5.7.0",
    "@vscode/test-electron": "^2.3.0",
    "@vscode/vsce": "^2.24.0"
  }
}
```

Nota: **No** se necesita React, Ink, ni ninguna dependencia de terminal UI. El webview usa vanilla TypeScript o Preact (opcional, muy liviano).
