use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs::read_to_string,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use anyhow::Context;
use clap::Parser;
use git_version::git_version;
use regex::Regex;
use tracing::{debug, info, warn};
use tree_sitter::{Node, TreeCursor};
use which::which;

#[derive(Debug, clap::Parser)]
#[command(version=git_version!())]
struct Cli {
    make_path: Option<PathBuf>,
    #[arg(default_value = "compile_commands.json")]
    compile_units: PathBuf,
    #[arg(short, long)]
    stdin: bool,
    #[arg(short, long)]
    warn_dup: bool,
    #[arg(short, long)]
    verbose: bool,
}
fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    tracing_subscriber::fmt()
        .without_time()
        .with_target(false)
        .with_max_level(if args.verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .init();

    let make_path = if args.stdin {
        let mut path_s = String::new();
        std::io::stdin().read_line(&mut path_s)?;
        PathBuf::from(path_s.trim())
    } else {
        if let Some(p) = &args.make_path {
            p.clone()
        } else {
            return Err(anyhow::anyhow!(
                "<make_file> path is required unless --stdin flag is set"
            ));
        }
    };

    let units = make_to_compile_units(make_path.as_path())?;
    let mut units_map = HashMap::new();
    if args.compile_units.exists() {
        let current_units: Vec<CompilationUnit> = serde_json::from_reader(
            std::fs::File::open(args.compile_units.as_path())
                .context(format!("Opening {}", args.compile_units.display()))?,
        )?;
        for unit in current_units {
            units_map.insert(unit.file.clone(), unit);
        }
        info!(
            "Got {} units from current compile_commands.json",
            units_map.len()
        );
    }
    info!("Merging {} new units", units.len());
    for unit in units {
        if args.warn_dup && units_map.contains_key(&unit.file) {
            warn!(
                "{} duplicated in compile_commands.json",
                unit.file.display()
            );
        }
        units_map.insert(unit.file.clone(), unit);
    }
    let new_units: Vec<CompilationUnit> = units_map.into_values().collect();
    info!("Writing {} units", new_units.len());
    let new_json_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(args.compile_units.as_path())
        .context(format!(
            "Opening {} for write",
            args.compile_units.display()
        ))?;
    serde_json::to_writer_pretty(new_json_file, &new_units)?;
    Ok(())
}

fn make_to_compile_units(p: impl AsRef<Path>) -> anyhow::Result<Vec<CompilationUnit>> {
    debug!("Parsing {}", p.as_ref().display());
    let org_make_content = read_to_string(p.as_ref())?;
    let make_content = nmake_preprocess(&org_make_content)?;
    let make_content = patch_error_syntax(&make_content);
    let make = MakeRecipes::parse_makefile(&make_content)?;
    let mut units = vec![];
    if let Some(gen_lib_cmds) = make.recipes.get("gen_libs") {
        for cmd in gen_lib_cmds {
            let cmd = cmd.replace("\\", "\\\\");
            let components = if let Some(comp) = comma::parse_command(&cmd) {
                comp
            } else {
                continue;
            };
            for comp in components {
                let fp = Path::new(comp.as_str());
                if fp.exists()
                    && (fp.extension() == Some(OsStr::new(".mak"))
                        || fp.file_name() == Some(OsStr::new("GNUmakefile"))
                        || fp.file_name() == Some(OsStr::new("Makefile")))
                {
                    let lib_units = make_to_compile_units(fp)?;
                    units.extend(lib_units.into_iter());
                }
            }
        }
    }

    let comp_units = make.generate_compilation_units()?;
    units.extend(comp_units.into_iter());
    Ok(units)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompilationUnit {
    /// The working directory of the compilation.
    /// All paths specified in the command or file
    /// fields must be either absolute or relative
    /// to this directory.
    directory: PathBuf,
    /// The main translation unit source processed
    /// by this compilation step. This is used by
    /// tools as the key into the compilation database.
    /// There can be multiple command objects for the
    /// same file, for example if the same source file
    /// is compiled with different configurations.
    file: PathBuf,
    /// The compile command argv as list of strings.
    /// This should run the compilation step for the
    /// translation unit `file`. `arguments[0]` should
    /// be the executable name, such as `clang++`.
    /// Arguments should not be escaped, but ready
    /// to pass to `execvp()`.
    arguments: Vec<String>,
    /// The name of the output created by this compilation
    /// step. This field is optional. It can be used
    /// to distinguish different processing modes of
    /// the same input file.
    output: Option<PathBuf>,
}

#[derive(Debug)]
struct MakeRecipes {
    #[allow(dead_code)]
    var_dict: HashMap<String, String>,
    recipes: HashMap<String, Vec<String>>,
    depex_dict: HashMap<String, HashSet<String>>,
}

impl MakeRecipes {
    fn parse_makefile(src: &str) -> anyhow::Result<Self> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser.set_language(&tree_sitter_make::LANGUAGE.into())?;
        let tree = ts_parser
            .parse(src.as_bytes(), None)
            .context("Not able to parse the makefile")?;
        let mut var_dict = HashMap::from_iter([(
            "WORKSPACE".to_string(),
            std::env::current_dir()
                .context("Not able to get current dir")?
                .as_os_str()
                .to_str()
                .context("Path contains non utf7 string")?
                .to_string(),
        )]);
        let mut recipes: HashMap<String, Vec<String>> = HashMap::new();
        let mut depex_dict: HashMap<String, HashSet<String>> = HashMap::new();

        for node in TsWalker::from(tree.walk()) {
            if node.kind() == "ERROR" {
                let start = node.range().start_point;
                return Err(anyhow::anyhow!(
                    "Error node found at {}:{}",
                    start.row,
                    start.column
                ));
            }
            match node.kind() {
                "rule" => {
                    let (target, depex, cmd) = parse_rule_node(node, src)?;
                    let target = solve_reference(target, &var_dict)?.trim().to_string();
                    if let Some(recipe) = recipes.get_mut(&target) {
                        recipe.extend(
                            cmd.into_iter()
                                .map(|cmd| solve_reference(&cmd, &var_dict))
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                    } else {
                        recipes.insert(
                            target.clone(),
                            cmd.into_iter()
                                .map(|cmd| solve_reference(&cmd, &var_dict))
                                .collect::<Result<Vec<_>, _>>()?,
                        );
                    }
                    if let Some(depex) = depex {
                        for target in target.split_whitespace() {
                            let depex = solve_reference(&depex, &var_dict)?;
                            if let Some(cur_depex) = depex_dict.get_mut(target) {
                                cur_depex.extend(depex.split_whitespace().map(|s| s.to_string()));
                            } else {
                                depex_dict.insert(
                                    target.to_string(),
                                    HashSet::from_iter(
                                        depex.split_whitespace().map(|s| s.to_string()),
                                    ),
                                );
                            }
                        }
                    }
                }
                "variable_assignment" => {
                    let (var_name, var_value) = parse_var_assign_node(node, src)?;
                    let var_value = var_value
                        .map(|val| solve_reference(&val, &var_dict))
                        .transpose()?;
                    if let Some(val) = var_value {
                        var_dict.insert(var_name.to_string(), val);
                    } else {
                        var_dict.remove(var_name);
                    }
                }
                _ => {}
            }
        }
        Ok(Self {
            var_dict,
            recipes,
            depex_dict,
        })
    }

    fn generate_compilation_units(&self) -> anyhow::Result<Vec<CompilationUnit>> {
        let mut units = vec![];

        for (targets, commands) in self.recipes.iter() {
            for target in targets.split_whitespace() {
                if !target.ends_with(".obj") {
                    continue;
                }
                let depex_set = if let Some(set) = self.depex_dict.get(target) {
                    set
                } else {
                    continue;
                };
                for depex in depex_set.iter() {
                    if !(depex.ends_with(".c") || depex.ends_with(".cpp")) {
                        continue;
                    }

                    for cmd in commands {
                        let mut arguments = comma::parse_command(&cmd.replace("\\", "/"))
                            .context("Not able to parse command arguments")?;
                        if !Path::new(&arguments[0]).is_absolute() {
                            let exec = which(arguments[0].as_str()).context(format!(
                                "Could not get absolute path for {}",
                                arguments[0]
                            ))?;
                            arguments[0] = exec
                                .as_os_str()
                                .to_str()
                                .context("Exectuable path contains non utf8 character")?
                                .to_string();
                        }

                        let comp_unit = CompilationUnit {
                            directory: std::env::current_dir()
                                .context("Cound not get current dir")?,
                            file: PathBuf::from(depex),
                            output: Some(PathBuf::from(target)),
                            arguments,
                        };
                        units.push(comp_unit);
                    }
                }
            }
        }

        Ok(units)
    }
}

static INCLUDE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)!INCLUDE\s+<?(.+)>?").expect("Construct regex failed"));
fn nmake_preprocess(src: &str) -> anyhow::Result<Cow<str>> {
    // Variable could be used to form the include path
    // But at this point we haven't parse variable yet.
    // TODO:
    // Need to overhual the enture parse flow in oder to support
    // !INCLUDE directive

    //     let mut new_str = String::new();
    //     let mut prev_end = 0;
    //
    //     for cap in INCLUDE_PATTERN.captures_iter(src) {
    //         let inc_path = cap.get(1).context("Unable to get capture group")?;
    //         let inc_content = std::fs::read_to_string(inc_path.as_str()).context(format!("Opening {}", inc_path.as_str()))?;
    //         new_str.push_str(&src[prev_end..cap.get(0).unwrap().start()]);
    //         new_str.push('\n');
    //         new_str.push_str(&inc_content);
    //         new_str.push('\n');
    //         prev_end = cap.get(0).unwrap().end();
    //     }
    //     if prev_end == 0 {
    //         Ok(Cow::Borrowed(src))
    //     } else {
    //         new_str.push_str(&src[prev_end..]);
    //         Ok(Cow::Owned(new_str))
    //     }
    Ok(INCLUDE_PATTERN.replace_all(src, "#$0"))
}

static VARIABLE_ASSIGNMENT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\w+ *= *((\\\n|.)*)").expect("Construct regex failed"));
static NON_NL_BACKSLASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\\([^\n\r])").expect("Construct regex failed"));
fn patch_error_syntax(src: &str) -> Cow<str> {
    let mut insert_back_slash_pos = vec![];
    let escape_chars = ['"', '(', ')'];
    let src = NON_NL_BACKSLASH.replace_all(src, "\\\\$1");

    for cap in VARIABLE_ASSIGNMENT_PATTERN.captures_iter(&src) {
        let value_match = if let Some(group) = cap.get(1) {
            group
        } else {
            continue;
        };

        let mut prev_ch = '\0';
        let mut in_ref = false;
        for (idx, ch) in value_match.as_str().chars().enumerate() {
            if in_ref {
                if ch == ')' {
                    in_ref = false;
                    prev_ch = ch;
                    continue;
                } else {
                    continue;
                }
            }

            if ch == '(' && prev_ch == '$' {
                in_ref = true;
                continue;
            }

            if escape_chars.contains(&ch) {
                insert_back_slash_pos.push(idx + value_match.start());
            }
            prev_ch = ch;
        }
    }

    if let Some(last_insert) = insert_back_slash_pos.last() {
        let mut new_str = String::with_capacity(src.len() + insert_back_slash_pos.len());
        let mut start_pos = 0;

        for insert_pos in insert_back_slash_pos.iter() {
            new_str.push_str(&src[start_pos..*insert_pos]);
            new_str.push('\\');
            start_pos = *insert_pos;
        }
        new_str.push_str(&src[*last_insert..]);
        Cow::Owned(new_str)
    } else {
        src
    }
}

static NEWLINE_ESC_SEQ_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\\\r\n\s*|\\\n\s*").expect("Not able to construct regex pattern")
});
static ESC_SEQ_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\\([\(\)"\\])"#).expect("Not able to construct regex pattern"));
fn unescape_str<'a>(src: &'a str) -> String {
    let str = NEWLINE_ESC_SEQ_PATTERN.replace_all(src, " ");
    let str = ESC_SEQ_PATTERN.replace_all(&str, "$1");
    str.to_string()
}
fn parse_var_assign_node<'a>(
    node: Node<'_>,
    src: &'a str,
) -> anyhow::Result<(&'a str, Option<String>)> {
    let var_name = node
        .named_child(0)
        .context("variable_assignment does not have variable name")?
        .utf8_text(src.as_bytes())?;
    let var_value = node
        .named_child(1)
        .map(|node| node.utf8_text(src.as_bytes()))
        .transpose()?
        .map(|val| {
            let str = unescape_str(val);
            str.to_string()
        });
    Ok((var_name, var_value))
}

fn parse_rule_node<'a>(
    node: Node<'_>,
    src: &'a str,
) -> anyhow::Result<(&'a str, Option<String>, Vec<String>)> {
    let mut walker = TsWalker::from(node.walk());
    let target = loop {
        if let Some(node) = walker.next() {
            if node.kind() == "targets" {
                break node.utf8_text(src.as_bytes())?;
            }
        } else {
            return Err(anyhow::anyhow!("No target node in rule"));
        }
    };

    let mut prereq = String::new();
    while let Some(node) = walker.next() {
        if node.kind() == "prerequisites" {
            prereq.push_str(node.utf8_text(src.as_bytes())?);
        } else if node.kind() == "recipe" {
            break;
        }
    }

    let mut commands = vec![];
    while let Some(node) = walker.next() {
        if node.kind() == "shell_text" {
            let cmd = node.utf8_text(src.as_bytes())?;
            commands.push(unescape_str(cmd).to_string());
        }
    }
    Ok((
        target,
        if prereq.is_empty() {
            None
        } else {
            Some(unescape_str(&prereq).to_string())
        },
        commands,
    ))
}

struct TsWalker<'c> {
    cursor: TreeCursor<'c>,
    end: bool,
}

impl<'c> From<TreeCursor<'c>> for TsWalker<'c> {
    fn from(value: TreeCursor<'c>) -> Self {
        Self {
            cursor: value,
            end: false,
        }
    }
}

impl<'c> Iterator for TsWalker<'c> {
    type Item = Node<'c>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.end {
            return None;
        }
        let node = self.cursor.node();

        if self.cursor.goto_first_child() {
            return Some(node);
        }
        if self.cursor.goto_next_sibling() {
            return Some(node);
        }
        if self.cursor.goto_parent() {
            while !self.cursor.goto_next_sibling() {
                if !self.cursor.goto_parent() {
                    self.end = true;
                    return Some(node);
                }
            }
            return Some(node);
        }
        self.end = true;
        return Some(node);
    }
}

static REF_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\((.+?)\)").expect("Not able to construct regex"));
fn solve_reference(src: &str, var_dict: &HashMap<String, String>) -> anyhow::Result<String> {
    let mut last_end = 0;
    let mut new_str = String::new();

    for cap in REF_PATTERN.captures_iter(src) {
        let ref_name = cap.get(1).context("No capture group in capture")?;
        let ref_val = var_dict.get(ref_name.as_str()).map_or("", |s| s.as_str());
        new_str.push_str(&src[last_end..cap.get(0).unwrap().start()]);
        new_str.push_str(ref_val);
        last_end = cap.get(0).unwrap().end();
    }
    new_str.push_str(&src[last_end..]);
    Ok(new_str)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_patch_error_syntax() -> anyhow::Result<()> {
        let non_esc_sample = "# comment\n\
            NAME = value\n\
            NAME2 = root/$(NAME)";
        assert_eq!(patch_error_syntax(non_esc_sample), non_esc_sample);

        let esc_sample = "# comment\n\
            NAME = \\value\n\
            NAME2 = $(NAME)\n\
            NAME3 = value \"test\" \\$(NAME) ((abc))\\\n\t\
            CCD";
        let esced_sample = "# comment\n\
            NAME = \\\\value\n\
            NAME2 = $(NAME)\n\
            NAME3 = value \\\"test\\\" \\\\$(NAME) \\(\\(abc\\)\\)\\\n\t\
            CCD";
        assert_eq!(patch_error_syntax(esc_sample), esced_sample);
        Ok(())
    }

    #[test]
    fn test_tree_walker() -> anyhow::Result<()> {
        let esced_sample = "# comment\n\
            NAME = value\n\
            NAME2 = $(NAME)\n\
            NAME3 = value \\\"test\\\" $(NAME) \\(\\(abc\\)\\)\n\
            all: dep1 dep2\n\t\
            echo hi1\n\t\
            echo hi2\n";

        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser.set_language(&tree_sitter_make::LANGUAGE.into())?;
        let tree = ts_parser
            .parse(esced_sample, None)
            .context("Not able to parse the makefile")?;
        let walker = TsWalker::from(tree.walk());
        let nodes = walker.collect::<Vec<_>>();
        dbg!(&nodes);
        assert_eq!(nodes.len(), 38);
        Ok(())
    }

    #[test]
    fn test_parse_rule_node() -> anyhow::Result<()> {
        let sample = "all: dep1 dep2\n\t\
        echo hi1\n\t\
        echo hi2\n";

        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser.set_language(&tree_sitter_make::LANGUAGE.into())?;
        let tree = ts_parser
            .parse(sample, None)
            .context("Not able to parse the input")?;
        let walker = TsWalker::from(tree.walk());
        for node in walker {
            if node.kind() == "rule" {
                let (target, prereq, cmd) = parse_rule_node(node, sample)?;
                assert_eq!(target, "all");
                assert_eq!(prereq, Some("dep1 dep2".to_string()));
                assert_eq!(cmd, vec!["echo hi1".to_string(), "echo hi2".to_string()]);
                return Ok(());
            }
        }
        panic!("No rule node found");
    }

    #[test]
    fn test_solve_reference() -> anyhow::Result<()> {
        let dict = HashMap::from_iter([("VAR".to_string(), "VAL".to_string())]);
        assert_eq!(
            solve_reference("ABC$(VAR)DDE$(VAR)DDD", &dict)?,
            "ABCVALDDEVALDDD"
        );
        assert_eq!(solve_reference("ABCDDD$(NON)EE", &dict)?, "ABCDDDEE");
        Ok(())
    }
}
