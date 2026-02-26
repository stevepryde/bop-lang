use crate::builtins::{error, expect_number};
use crate::error::BopError;
use crate::value::{Value, values_equal};

/// Returns (return_value, optional_mutated_object)
pub fn array_method(
    arr: &[Value],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Number(arr.len() as f64), None)),
        "push" => {
            if args.len() != 1 {
                return Err(error(line, ".push() needs exactly 1 argument"));
            }
            let mut new_arr = arr.to_vec();
            new_arr.push(args[0].clone());
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "pop" => {
            let mut new_arr = arr.to_vec();
            let popped = new_arr.pop().unwrap_or(Value::None);
            Ok((popped, Some(Value::new_array(new_arr))))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs exactly 1 argument"));
            }
            let found = arr.iter().any(|v| values_equal(v, &args[0]));
            Ok((Value::Bool(found), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs exactly 1 argument"));
            }
            let idx = arr.iter().position(|v| values_equal(v, &args[0]));
            Ok((Value::Number(idx.map_or(-1.0, |i| i as f64)), None))
        }
        "insert" => {
            if args.len() != 2 {
                return Err(error(line, ".insert() needs 2 arguments: index and value"));
            }
            let i = expect_number("insert", &args[0], line)? as usize;
            let mut new_arr = arr.to_vec();
            if i > new_arr.len() {
                return Err(error(line, format!("Insert index {} is out of bounds", i)));
            }
            new_arr.insert(i, args[1].clone());
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "remove" => {
            if args.len() != 1 {
                return Err(error(line, ".remove() needs exactly 1 argument (index)"));
            }
            let i = expect_number("remove", &args[0], line)? as usize;
            let mut new_arr = arr.to_vec();
            if i >= new_arr.len() {
                return Err(error(line, format!("Remove index {} is out of bounds", i)));
            }
            let removed = new_arr.remove(i);
            Ok((removed, Some(Value::new_array(new_arr))))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_number("slice", &args[0], line)? as usize;
            let end = (expect_number("slice", &args[1], line)? as usize).min(arr.len());
            let start = start.min(end);
            let slice = arr[start..end].to_vec();
            Ok((Value::new_array(slice), None))
        }
        "reverse" => {
            let mut new_arr = arr.to_vec();
            new_arr.reverse();
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "sort" => {
            let mut new_arr = arr.to_vec();
            new_arr.sort_by(|a, b| match (a, b) {
                (Value::Number(x), Value::Number(y)) => {
                    x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal)
                }
                (Value::Str(x), Value::Str(y)) => x.cmp(y),
                _ => std::cmp::Ordering::Equal,
            });
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "join" => {
            if args.len() != 1 {
                return Err(error(line, ".join() needs exactly 1 argument (separator)"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".join() separator must be a string")),
            };
            let result = arr
                .iter()
                .map(|v| format!("{}", v))
                .collect::<Vec<_>>()
                .join(sep);
            Ok((Value::new_str(result), None))
        }
        _ => Err(error(line, format!("Array doesn't have a .{}() method", method))),
    }
}

pub fn string_method(
    s: &str,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Number(s.chars().count() as f64), None)),
        "contains" => {
            if args.len() != 1 {
                return Err(error(line, ".contains() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".contains() needs a string argument")),
            };
            Ok((Value::Bool(s.contains(substr)), None))
        }
        "starts_with" => {
            if args.len() != 1 {
                return Err(error(line, ".starts_with() needs 1 argument"));
            }
            let prefix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".starts_with() needs a string")),
            };
            Ok((Value::Bool(s.starts_with(prefix)), None))
        }
        "ends_with" => {
            if args.len() != 1 {
                return Err(error(line, ".ends_with() needs 1 argument"));
            }
            let suffix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".ends_with() needs a string")),
            };
            Ok((Value::Bool(s.ends_with(suffix)), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".index_of() needs a string")),
            };
            let idx = s.find(substr).map_or(-1.0, |i| i as f64);
            Ok((Value::Number(idx), None))
        }
        "split" => {
            if args.len() != 1 {
                return Err(error(line, ".split() needs 1 argument"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".split() needs a string")),
            };
            let parts: Vec<Value> = s
                .split(sep)
                .map(|p| Value::new_str(p.to_string()))
                .collect();
            Ok((Value::new_array(parts), None))
        }
        "replace" => {
            if args.len() != 2 {
                return Err(error(line, ".replace() needs 2 arguments: old and new"));
            }
            let old = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let new = match &args[1] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let result = s.replace(old, new);
            Ok((Value::new_str(result), None))
        }
        "upper" => {
            let result = s.to_uppercase();
            Ok((Value::new_str(result), None))
        }
        "lower" => {
            let result = s.to_lowercase();
            Ok((Value::new_str(result), None))
        }
        "trim" => {
            let result = s.trim().to_string();
            Ok((Value::new_str(result), None))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_number("slice", &args[0], line)? as usize;
            let chars: Vec<char> = s.chars().collect();
            let end = (expect_number("slice", &args[1], line)? as usize).min(chars.len());
            let start = start.min(end);
            let result: String = chars[start..end].iter().collect();
            Ok((Value::new_str(result), None))
        }
        _ => Err(error(line, format!("String doesn't have a .{}() method", method))),
    }
}

pub fn dict_method(
    entries: &[(String, Value)],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Number(entries.len() as f64), None)),
        "keys" => {
            let keys: Vec<Value> = entries
                .iter()
                .map(|(k, _)| Value::new_str(k.clone()))
                .collect();
            Ok((Value::new_array(keys), None))
        }
        "values" => {
            let vals: Vec<Value> = entries.iter().map(|(_, v)| v.clone()).collect();
            Ok((Value::new_array(vals), None))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs 1 argument"));
            }
            let key = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".has() needs a string key")),
            };
            Ok((Value::Bool(entries.iter().any(|(k, _)| k == key)), None))
        }
        _ => Err(error(line, format!("Dict doesn't have a .{}() method", method))),
    }
}

pub fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "push" | "pop" | "insert" | "remove" | "reverse" | "sort"
    )
}
