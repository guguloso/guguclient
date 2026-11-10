# Gugu Client for Roblox

Custom Roblox launcher & client built with Electron, featuring Discord Rich Presence integration and automatic GitHub Releases updates.

## Estrutura do Projeto

- `temp_fix/` — Código-fonte principal e recursos do launcher (`main`, `preload`, `renderer`, `resources`).
- `build/` — Scripts de build portáteis e geração de ícones (`repack_and_build.ps1`, `build-portable.ps1`, `PortableLauncher.cs`).

## Como compilar uma nova versão (ex: 1.0.17)

1. Execute o script de build passando o número da nova versão:
   ```powershell
   .\build\repack_and_build.ps1 -Version 1.0.17
   ```
2. O executável atualizado será gerado na raiz como:
   `GuguClientForRoblox-Portable-1.0.17.exe`

## Como publicar uma atualização (Auto-Updater)

O botão **Check for Updates** do launcher verifica automaticamente a última release publicada neste repositório (`guguloso/guguclient`).

1. Vá na aba **Releases** do repositório e clique em **Draft a new release**.
2. Crie uma tag com a nova versão (exemplo: `v1.0.17`).
3. Anexe o arquivo **`GuguClientForRoblox-Portable-1.0.17.exe`** gerado pelo script.
4. Clique em **Publish release**.

Todos os usuários em versões anteriores receberão e instalarão a atualização automaticamente ao clicar em **Check for Updates** ou ao abrir o launcher!
