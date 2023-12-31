use std::cmp::min;
use glob::Pattern;
use log::{LevelFilter};
use sha1::Sha1;
use sha1::Digest;
use url::Url;

pub fn sha1_hash(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha1::new();
    hasher.update(data);
    let result = hasher.finalize();
    return hex::encode(&result[..]).as_bytes().to_vec();
}
pub fn sha1_password_hash(password: &[u8], nonce: &[u8]) -> Vec<u8> {
    let mut hash = sha1_hash(password);
    let mut nonce_pass= nonce.to_vec();
    nonce_pass.append(&mut hash);
    return sha1_hash(&nonce_pass);
}
pub fn join_path(p1: &str, p2: &str) -> String {
    if p1.is_empty() && p2.is_empty() {
        "".to_string()
    } else if p1.is_empty() {
        p2.to_string()
    } else if p2.is_empty() {
        p1.to_string()
    } else {
        p1.to_string() + "/" + p2
    }
}

pub fn parse_log_verbosity<'a>(verbosity: &'a str, module_path: &'a str) -> Vec<(&'a str, LevelFilter)> {
    let mut ret: Vec<(&str, LevelFilter)> = Vec::new();
    for module_level_str in verbosity.split(',') {
        let module_level: Vec<_> = module_level_str.split('%').collect();
        let name = *module_level.get(0).unwrap_or(&".");
        let level = *module_level.get(1).unwrap_or(&"D");
        let module = if name == "." { module_path } else { name };
        let level = match level {
            "E" => LevelFilter::Error,
            "W" => LevelFilter::Warn,
            "I" => LevelFilter::Info,
            "D" => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        };
        ret.push((module, level));
    }
    ret
}

pub fn login_from_url(url: &Url) -> (String, String) {
    let password = if let Some(password) = url.password() {
        password.to_owned()
    } else {
        let mut password = None;
        for (key,val) in url.query_pairs() {
            if key == "password" {
                password = Some(val.to_string());
                break;
            }
        }
        password.unwrap_or_default()
    };
    let user = url.username().to_string();
    (user, password)
}

pub fn glob_len(glob: &str) -> usize {
    // number of slashes + 1
    glob.split('/').count()
}
pub fn left_glob(glob: &str, glob_len: usize) -> Option<&str> {
    let mut ix: usize = 0;
    let mut n: usize = 0;
    for p in glob.splitn(glob_len + 1, '/') {
        ix += p.len();
        n += 1;
        if n == glob_len {
            break
        }
    }
    if n == glob_len {
        ix += n - 1; // add intermediate slashes
        Some(&glob[0..ix])
    } else {
        None
    }
}
pub fn split_glob_on_match<'a, 'b>(glob_pattern: &'a str, shv_path: &'b str) -> Result<Option<(&'a str, &'a str)>, String> {
    if glob_pattern.is_empty() {
        return Ok(None);
    }
    // find first '**' occurrence in paths
    let globstar_pos = glob_pattern.find("**");
    let pattern1 = match globstar_pos {
        None => { glob_pattern }
        Some(ix) => {
            if ix == 0 { "" } else { &glob_pattern[0 .. (ix - 1)] }
        }
    };
    if globstar_pos.is_some() && pattern1.is_empty() {
        // paths starts with **, this matches everything
        return Ok(Some(("**", glob_pattern)))
    }
    if pattern1.is_empty() { return Err("Valid glob pattern cannot be empty".into()) };
    if shv_path.is_empty() { return Err("Valid mount point cannot be empty".into()) };
    let shv_path_glen = glob_len(shv_path);
    let pattern1_glen = glob_len(pattern1);
    let match_len = min(shv_path_glen, pattern1_glen);
    let trimmed_pattern1 = left_glob(pattern1, match_len).unwrap();
    let trimmed_path = left_glob(shv_path, match_len).unwrap();
    let pattern = Pattern::new(trimmed_pattern1).map_err(|err| err.to_string())?;
    if pattern.matches(trimmed_path) {
        match globstar_pos {
            None => {
                if shv_path_glen > pattern1_glen {
                    // a/b vs a/b/c
                    Ok(None)
                } else if shv_path_glen == pattern1_glen {
                    // a/b/c vs a/b/c
                    Ok(Some((trimmed_pattern1, "")))
                } else {
                    // a/b/c vs a/b
                    Ok(Some((trimmed_pattern1, &glob_pattern[(trimmed_pattern1.len()+1) .. ])))
                }
            }
            Some(ix) => {
                if shv_path_glen > pattern1_glen {
                    // a/b/** vs a/b/c
                    Ok(Some((&glob_pattern[0 .. (ix+2)], &glob_pattern[ix ..])))
                } else {
                    // a/b/c/** vs a/b/c
                    // a/b/c/d/** vs a/b/c
                    Ok(Some((trimmed_pattern1, &glob_pattern[trimmed_pattern1.len()+1 ..])))
                }
            }
        }
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::util::{glob_len, left_glob, split_glob_on_match};

    #[test]
    fn test_glob_len() {
        let data = vec![
            ("", 1usize),
            ("/", 2usize),
            ("a", 1usize),
            ("a/b/c", 3usize),
            ("a/b/", 3usize),
        ];
        for (g, n) in data {
            assert_eq!(glob_len(g), n);
        }
    }
    #[test]
    fn test_left_glob() {
        let data = vec![
            ("", 1usize, Some("")),
            ("a", 1usize, Some("a")),
            ("a", 2usize, None),
            ("a/b", 1usize, Some("a")),
            ("a/b", 2usize, Some("a/b")),
            ("a/b", 3usize, None),
        ];
        for (glob, len, trimmed) in data {
            assert_eq!(left_glob(glob, len), trimmed);
        }
    }
    #[test]
    fn test_split_glob_on_match() {
        let data = vec![
            ("", "a/b/c", None),
            ("a", "a/b/c", None),
            ("a/b", "a/b/c", None),
            ("a/b/c", "a/b/c", Some(("a/b/c", ""))),
            ("a/b/c/d", "a/b/c", Some(("a/b/c", "d"))),
            ("a/b/c", "a", Some(("a", "b/c"))),
            ("a/b/c", "a/b", Some(("a/b", "c"))),
            ("a/b/c", "a/b/c/d", None),
            ("a/b/c", "a/b/d", None),
            ("**", "a/b/c", Some(("**", "**"))),
            ("a/**", "a/b/c", Some(("a/**", "**"))),
            ("a/**/c", "a/b/c", Some(("a/**", "**/c"))),
            ("a/b/c/**", "a/b/c", Some(("a/b/c", "**"))),
            ("a/b*/c/**", "a/b/c", Some(("a/b*/c", "**"))),
            ("?/b*/c/**", "a/b/c", Some(("?/b*/c", "**"))),
            ("a/b/c/**/d/e/**", "a/b/c", Some(("a/b/c", "**/d/e/**"))),
            ("**/a/b", "a/b/c", Some(("**", "**/a/b"))),
        ];
        for (glob, path, result) in data {
            assert_eq!(split_glob_on_match(glob, path), Ok(result));
        }
    }
}
