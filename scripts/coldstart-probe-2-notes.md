Round 2 of the cold-start probes, for finding where Rune's GPU setup spends its time on a Windows machine. **Not a release of Arbor.**

- `gpu_probe.exe` (console): Rune's own window path phase by phase (its "is there a DX12 adapter" probe, the surface, the adapter pick and device, `Renderer::new` with the precompiled DXBC uber shader and again compiling the WGSL with FXC, the first present), then plain wgpu on DX12 and on Vulkan, per adapter.
- `arbor.exe`: this round's Arbor: the GPU device and renderer made on a thread from the top of `main` (`rune::warm_gpu`), and built from a Rune checkout with its committed LF line endings, so Rune loads its precompiled DXBC uber shader instead of compiling it with FXC at every launch (round 1's and v0.1.0-alpha.4's builds had CRLF, which silently turned that off).
- `arbor-base.exe`: round 1's arbor.exe (v0.1.0-alpha.4's startup), for comparison on the same machine.
- `rune-hello.exe`, `win32-hello.exe`: as in round 1; rune-hello also reports when Rune started the GPU device, created the window, and first drew.

MSVC `dist` builds by the "Cold start (Windows)" workflow. Run in PowerShell (Windows PowerShell 5.1 or PowerShell 7, no admin); it takes about a minute and opens and closes windows:

```powershell
irm https://raw.githubusercontent.com/smrdotgg/arbor/coldstart-probe-2/scripts/coldstart-probe-2.ps1 | iex
```

It downloads this zip to a new folder in %TEMP% and prints everything to paste back. Arbor opens your home folder with your settings (read, not changed).
