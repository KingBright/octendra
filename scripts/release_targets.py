"""Run authorized target pipelines independently. One failure never hides a peer's acceptance."""
from concurrent.futures import ThreadPoolExecutor, as_completed

DEFAULT_PLATFORMS = ('macos', 'linux', 'windows')
PLATFORMS = (*DEFAULT_PLATFORMS, 'linux-arm64')
BINARY_PATHS = {
    'octendra-code-macos-arm64': ('macos', 'release/octendra-code'),
    'octendra-code-linux-amd64': ('linux', 'x86_64-unknown-linux-musl/release/octendra-code'),
    'octendra-code-windows-amd64.exe': ('windows', 'x86_64-pc-windows-msvc/release/octendra-code.exe'),
    'octendra-macos-arm64': ('macos', 'release/octendra'),
    'octendra-linux-amd64': ('linux', 'x86_64-unknown-linux-musl/release/octendra'),
    'octendra-windows-amd64.exe': ('windows', 'x86_64-pc-windows-msvc/release/octendra.exe'),
    'octendra-code-linux-arm64': ('linux-arm64', 'aarch64-unknown-linux-musl/release/octendra-code'),
    'octendra-linux-arm64': ('linux-arm64', 'aarch64-unknown-linux-musl/release/octendra'),
}
BINARY_PLATFORMS = {name: value[0] for name, value in BINARY_PATHS.items()}


def selected_platforms(values=None):
    selected = DEFAULT_PLATFORMS if values is None else tuple(values)
    if not selected or len(set(selected)) != len(selected) or not set(selected).issubset(PLATFORMS):
        raise ValueError('invalid release platform selection')
    return tuple(name for name in PLATFORMS if name in selected)


def code_artifact(platform, architecture):
    """Never infer architecture from a path or substitute another machine type."""
    arch = {'aarch64': 'arm64', 'arm64': 'arm64', 'x86_64': 'amd64', 'amd64': 'amd64'}.get(
        str(architecture or '').lower())
    if (platform, arch) not in {('macos', 'arm64'), ('linux', 'amd64'),
                                ('linux', 'arm64'), ('windows', 'amd64')}:
        raise ValueError('unsupported or missing target architecture')
    return 'octendra-code-' + platform + '-' + arch + ('.exe' if platform == 'windows' else '')

def selected(targets, action, on_result, max_workers=2):
    if not targets or len(set(targets))!=len(targets) or max_workers<1:
        raise ValueError('unique nonempty targets required')
    results={}
    with ThreadPoolExecutor(max_workers=min(max_workers,len(targets))) as pool:
        futures={pool.submit(action,t):t for t in targets}
        for future in as_completed(futures):
            target=futures[future]
            try:
                result=future.result()
                if not isinstance(result,dict) or 'state' not in result:raise ValueError('target has no factual state')
            except Exception as error:
                result={'state':'needs_recovery','failure_type':type(error).__name__,'recovery':'inspect original target receipts; successful peers remain installed'}
            results[target]=result
            on_result(target,result)
    return {'targets':results,'all_targets_accepted':all(v['state']=='accepted' for v in results.values()),
            'selected_targets':list(targets),'scope':'original target set preserved; independent pipelines'}
