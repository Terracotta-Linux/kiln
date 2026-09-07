//! `kiln completions <shell>`. Static, hand-written scripts — matching
//! `args.rs`, the command surface is small and fixed enough that a
//! generator would not make these clearer, and it would be one more
//! dependency to keep in sync with a parser that already isn't one.

pub const SHELLS: &[&str] = &["bash", "zsh", "fish"];

pub fn script(shell: &str) -> Result<&'static str, String> {
    match shell {
        "bash" => Ok(BASH),
        "zsh" => Ok(ZSH),
        "fish" => Ok(FISH),
        other => Err(format!(
            "unknown shell `{other}`; kiln completions supports {}",
            SHELLS.join(", ")
        )),
    }
}

const BASH: &str = r#"# kiln(1) completion                                -*- shell-script -*-
_kiln() {
    local cur prev words cword
    _init_completion || return

    local verbs="check build apply rebuild explain show diff why owns list \
status rollback deploy pin unpin rm clean init sysroot completions help version"
    local global="--config --sysroot --allow-external-sources --module-root \
--verbose --version --help"

    case "$prev" in
        --config|-c|--sysroot|--module-root)
            _filedir -d
            return
            ;;
        --keep)
            return
            ;;
        sysroot)
            COMPREPLY=($(compgen -W "init" -- "$cur"))
            return
            ;;
        completions)
            COMPREPLY=($(compgen -W "bash zsh fish" -- "$cur"))
            return
            ;;
    esac

    local verb=
    local w
    for w in "${words[@]:1:cword-1}"; do
        case "$w" in
            -*) ;;
            *) verb="$w"; break ;;
        esac
    done

    if [[ -z "$verb" ]]; then
        if [[ "$cur" == -* ]]; then
            COMPREPLY=($(compgen -W "$global" -- "$cur"))
        else
            COMPREPLY=($(compgen -W "$verbs" -- "$cur"))
        fi
        return
    fi

    case "$verb" in
        check) COMPREPLY=($(compgen -W "--offline --deep" -- "$cur")) ;;
        build|apply) COMPREPLY=($(compgen -W "--force --offline --keep-failed" -- "$cur")) ;;
        clean) COMPREPLY=($(compgen -W "--keep --dry-run --remove-baseline" -- "$cur")) ;;
        rm) COMPREPLY=($(compgen -W "--remove-baseline" -- "$cur")) ;;
        *) COMPREPLY=($(compgen -W "$global" -- "$cur")) ;;
    esac
}
complete -F _kiln kiln
"#;

const ZSH: &str = r#"#compdef kiln
# kiln(1) completion

_kiln() {
    local -a verbs global_flags
    verbs=(
        'check:what would change, without building'
        'build:build an image'
        'apply:build, then stage for next boot'
        'rebuild:rebuild a past generation from its record'
        'explain:which file set a config value'
        'show:the merged manifest, or a past generation'
        'diff:what changed between two generations'
        'why:what pulled a package into the image'
        'owns:which package owns a file in the image'
        'list:every generation on this machine'
        'status:what is booted, what boots next, /etc drift'
        'rollback:boot the previous generation'
        'deploy:boot a specific generation'
        'pin:keep a generation through kiln clean'
        'unpin:let a generation be cleaned again'
        'rm:undeploy generations'
        'clean:keep N, the baseline, and anything pinned'
        'init:scaffold /etc/kiln'
        'sysroot:create an OSTree sysroot to build into'
        'completions:print a shell completion script'
        'help:show usage'
        'version:print the version'
    )
    global_flags=(
        '(-c --config)'{-c,--config}'[entry point, or a directory with system.toml]:path:_files -/'
        '--sysroot[operate on another root]:path:_files -/'
        '--allow-external-sources[permit sources outside the config root]'
        '--module-root[override /usr/share/kiln/modules]:path:_files -/'
        '(-v --verbose)'{-v,--verbose}'[more detail]'
        '(-V --version)'{-V,--version}'[print the version and exit]'
        '(-h --help)'{-h,--help}'[show usage]'
    )

    _arguments -C $global_flags '1: :->verb' '*:: :->rest' && return

    case $state in
        verb) _describe 'command' verbs ;;
        rest)
            case ${words[1]} in
                check) _arguments '--offline' '--deep' ;;
                build|apply) _arguments '--force' '--offline' '--keep-failed' ;;
                clean) _arguments '--keep[generations to keep]:count' '--dry-run' '--remove-baseline' ;;
                rm) _arguments '--remove-baseline' ;;
                sysroot) (( CURRENT == 2 )) && _values 'subcommand' 'init[create an OSTree sysroot]' ;;
                completions) (( CURRENT == 2 )) && _values 'shell' bash zsh fish ;;
            esac
            ;;
    esac
}

_kiln "$@"
"#;

const FISH: &str = r#"# kiln(1) completions
set -l __kiln_verbs check build apply rebuild explain show diff why owns \
    list status rollback deploy pin unpin rm clean init sysroot completions \
    help version

complete -c kiln -f
complete -c kiln -n "not __fish_seen_subcommand_from $__kiln_verbs" -a "$__kiln_verbs"

complete -c kiln -s c -l config -d 'entry point, or a directory with system.toml' -rF
complete -c kiln -l sysroot -d 'operate on another root' -rF
complete -c kiln -l allow-external-sources -d 'permit sources outside the config root'
complete -c kiln -l module-root -d 'override /usr/share/kiln/modules' -rF
complete -c kiln -s v -l verbose -d 'more detail'
complete -c kiln -s V -l version -d 'print the version and exit'
complete -c kiln -s h -l help -d 'show usage'

complete -c kiln -n "__fish_seen_subcommand_from check" -l offline
complete -c kiln -n "__fish_seen_subcommand_from check" -l deep
complete -c kiln -n "__fish_seen_subcommand_from build apply" -l force
complete -c kiln -n "__fish_seen_subcommand_from build apply" -l offline
complete -c kiln -n "__fish_seen_subcommand_from build apply" -l keep-failed
complete -c kiln -n "__fish_seen_subcommand_from clean" -l keep -rF
complete -c kiln -n "__fish_seen_subcommand_from clean" -l dry-run
complete -c kiln -n "__fish_seen_subcommand_from clean rm" -l remove-baseline
complete -c kiln -n "__fish_seen_subcommand_from sysroot; and not __fish_seen_subcommand_from init" -a init
complete -c kiln -n "__fish_seen_subcommand_from completions" -a "bash zsh fish"
"#;
