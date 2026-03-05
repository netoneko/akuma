use crate::context::ShellContext;
use crate::parse::{
    expand_variables, parse_command_chain, parse_command_line, parse_pipeline, ChainOperator,
    RedirectMode,
};
use crate::registry::CommandRegistry;
use crate::types::VecWriter;
use crate::util::{split_first_word, trim_bytes, translate_input_keys};
use crate::parse::parse_args;

mod pipeline_tests {
    use super::*;

    #[test]
    fn single_stage() {
        let stages = parse_pipeline(b"echo hello");
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0], b"echo hello");
    }

    #[test]
    fn two_stages() {
        let stages = parse_pipeline(b"cat file | grep hello");
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0], b"cat file");
        assert_eq!(stages[1], b"grep hello");
    }

    #[test]
    fn three_stages() {
        let stages = parse_pipeline(b"ls | sort | head");
        assert_eq!(stages.len(), 3);
    }

    #[test]
    fn empty_input() {
        let stages = parse_pipeline(b"");
        assert!(stages.is_empty());
    }
}

mod chain_tests {
    use super::*;

    #[test]
    fn semicolon_chain() {
        let chain = parse_command_chain(b"echo a; echo b");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].command, b"echo a");
        assert_eq!(chain[0].next_operator, Some(ChainOperator::Semicolon));
        assert_eq!(chain[1].command, b"echo b");
        assert!(chain[1].next_operator.is_none());
    }

    #[test]
    fn and_chain() {
        let chain = parse_command_chain(b"true && echo ok");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].next_operator, Some(ChainOperator::And));
    }

    #[test]
    fn single_command() {
        let chain = parse_command_chain(b"echo hello");
        assert_eq!(chain.len(), 1);
        assert!(chain[0].next_operator.is_none());
    }
}

mod redirect_tests {
    use super::*;

    #[test]
    fn overwrite() {
        let parsed = parse_command_line(b"echo hi > file.txt");
        assert_eq!(parsed.redirect_mode, RedirectMode::Overwrite);
        assert_eq!(parsed.redirect_target, Some(&b"file.txt"[..]));
        assert_eq!(parsed.stages.len(), 1);
        assert_eq!(parsed.stages[0], b"echo hi");
    }

    #[test]
    fn append() {
        let parsed = parse_command_line(b"echo hi >> file.txt");
        assert_eq!(parsed.redirect_mode, RedirectMode::Append);
        assert_eq!(parsed.redirect_target, Some(&b"file.txt"[..]));
    }

    #[test]
    fn no_redirect() {
        let parsed = parse_command_line(b"echo hi");
        assert_eq!(parsed.redirect_mode, RedirectMode::None);
        assert!(parsed.redirect_target.is_none());
    }
}

mod util_tests {
    use super::*;

    #[test]
    fn trim() {
        assert_eq!(trim_bytes(b"  hello  "), b"hello");
        assert_eq!(trim_bytes(b"hello"), b"hello");
        assert_eq!(trim_bytes(b"   "), b"");
        assert_eq!(trim_bytes(b""), b"");
    }

    #[test]
    fn split_word() {
        let (first, rest) = split_first_word(b"echo hello world");
        assert_eq!(first, b"echo");
        assert_eq!(rest, b"hello world");
    }

    #[test]
    fn split_word_no_rest() {
        let (first, rest) = split_first_word(b"echo");
        assert_eq!(first, b"echo");
        assert!(rest.is_empty());
    }

    #[test]
    fn translate_delete_key() {
        let input = [0x1b, b'[', b'3', b'~'];
        let result = translate_input_keys(&input);
        assert_eq!(result, [0x7f]);
    }

    #[test]
    fn translate_passthrough() {
        let result = translate_input_keys(b"abc");
        assert_eq!(result, b"abc");
    }
}

mod expand_tests {
    use super::*;

    #[test]
    fn dollar_var() {
        let ctx = ShellContext::with_defaults(&["FOO=bar"], false);
        let result = expand_variables(b"echo $FOO", &ctx);
        assert_eq!(result, b"echo bar");
    }

    #[test]
    fn braced_var() {
        let ctx = ShellContext::with_defaults(&["X=42"], false);
        let result = expand_variables(b"val=${X}px", &ctx);
        assert_eq!(result, b"val=42px");
    }

    #[test]
    fn tilde_expansion() {
        let ctx = ShellContext::with_defaults(&["HOME=/root"], false);
        let result = expand_variables(b"cd ~/docs", &ctx);
        assert_eq!(result, b"cd /root/docs");
    }

    #[test]
    fn single_quote_no_expand() {
        let ctx = ShellContext::with_defaults(&["X=1"], false);
        let result = expand_variables(b"echo '$X'", &ctx);
        assert_eq!(result, b"echo '$X'");
    }

    #[test]
    fn double_dollar() {
        let ctx = ShellContext::with_defaults(&[], false);
        let result = expand_variables(b"echo $$", &ctx);
        assert_eq!(result, b"echo $");
    }
}

mod parse_args_tests {
    use super::*;

    #[test]
    fn simple() {
        let args = parse_args(b"hello world");
        assert_eq!(args, ["hello", "world"]);
    }

    #[test]
    fn quoted() {
        let args = parse_args(b"echo \"hello world\"");
        assert_eq!(args, ["echo", "hello world"]);
    }

    #[test]
    fn empty() {
        let args = parse_args(b"");
        assert!(args.is_empty());
    }
}

mod context_tests {
    use super::*;

    #[test]
    fn resolve_absolute_path() {
        let ctx = ShellContext::with_defaults(&[], false);
        assert_eq!(ctx.resolve_path("/usr/bin"), "/usr/bin");
    }

    #[test]
    fn resolve_relative_path() {
        let mut ctx = ShellContext::with_defaults(&[], false);
        ctx.set_cwd("/home");
        assert_eq!(ctx.resolve_path("docs"), "/home/docs");
    }

    #[test]
    fn resolve_dotdot() {
        let mut ctx = ShellContext::with_defaults(&[], false);
        ctx.set_cwd("/home/user");
        assert_eq!(ctx.resolve_path(".."), "/home");
    }

    #[test]
    fn env_round_trip() {
        let mut ctx = ShellContext::with_defaults(&[], false);
        ctx.set_env("KEY", "value");
        assert_eq!(ctx.get_env("KEY"), Some("value"));
        ctx.remove_env("KEY");
        assert_eq!(ctx.get_env("KEY"), None);
    }
}

mod registry_tests {
    use super::*;
    use alloc::boxed::Box;
    use core::future::Future;
    use core::pin::Pin;
    use crate::types::{Command, ShellError};

    struct DummyCmd;
    impl Command for DummyCmd {
        fn name(&self) -> &'static str { "dummy" }
        fn aliases(&self) -> &'static [&'static str] { &["dm"] }
        fn description(&self) -> &'static str { "A dummy command" }
        fn execute<'a>(
            &'a self, _args: &'a [u8], _stdin: Option<&'a [u8]>,
            _stdout: &'a mut VecWriter, _ctx: &'a mut ShellContext,
        ) -> Pin<Box<dyn Future<Output = Result<(), ShellError>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }
    static DUMMY: DummyCmd = DummyCmd;

    #[test]
    fn find_by_name() {
        let mut reg = CommandRegistry::new();
        reg.register(&DUMMY);
        assert!(reg.find(b"dummy").is_some());
    }

    #[test]
    fn find_by_alias() {
        let mut reg = CommandRegistry::new();
        reg.register(&DUMMY);
        assert!(reg.find(b"dm").is_some());
    }

    #[test]
    fn not_found() {
        let reg = CommandRegistry::new();
        assert!(reg.find(b"missing").is_none());
    }
}

mod vec_writer_tests {
    use super::*;

    #[test]
    fn collects_bytes() {
        let w = VecWriter::new();
        let inner = w.into_inner();
        assert!(inner.is_empty());

        let default = VecWriter::default();
        assert_eq!(default.as_slice().len(), 0);
    }
}
