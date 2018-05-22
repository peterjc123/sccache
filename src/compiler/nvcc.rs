// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![allow(unused_imports,dead_code,unused_variables)]

use ::compiler::{
    gcc,
    Cacheable,
    CompilerArguments,
    write_temp_file,
};
use compiler::args::*;
use compiler::c::{CCompilerImpl, CCompilerKind, Language, ParsedArguments};
use compiler::gcc::GCCArgAttribute::*;
use log::LogLevel::Trace;
use futures::future::{self, Future};
use futures_cpupool::CpuPool;
use mock_command::{
    CommandCreator,
    CommandCreatorSync,
    RunCommand,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{
    self,
    Write,
};
use std::path::Path;
use std::process;
use util::{run_input_output, OsStrExt};

use errors::*;

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct NVCC;

impl CCompilerImpl for NVCC {
    fn kind(&self) -> CCompilerKind { CCompilerKind::NVCC }
    fn parse_arguments(&self,
                       arguments: &[OsString],
                       cwd: &Path) -> CompilerArguments<ParsedArguments>
    {
        gcc::parse_arguments(arguments, cwd, (&gcc::ARGS[..], &ARGS[..]))
    }

    fn preprocess<T>(&self,
                     creator: &T,
                     executable: &Path,
                     parsed_args: &ParsedArguments,
                     cwd: &Path,
                     env_vars: &[(OsString, OsString)])
                     -> SFuture<process::Output> where T: CommandCreatorSync
    {
        trace!("preprocess");
        let language = match parsed_args.language {
            Language::C => "c",
            Language::Cxx => "c++",
            Language::ObjectiveC => "objective-c",
            Language::ObjectiveCxx => "objective-c++",
        };
        let mut cmd = creator.clone().new_command_sync(executable);
        cmd.arg("-E")
            .arg(&parsed_args.input)
            .args(&parsed_args.preprocessor_args)
            .args(&parsed_args.common_args)
            .env_clear()
            .envs(env_vars.iter().map(|&(ref k, ref v)| (k, v)))
            .current_dir(cwd);

        if log_enabled!(Trace) {
            trace!("preprocess: {:?}", cmd);
        }
        run_input_output(cmd, None)
    }

    fn compile<T>(&self,
                  creator: &T,
                  executable: &Path,
                  parsed_args: &ParsedArguments,
                  cwd: &Path,
                  env_vars: &[(OsString, OsString)])
                  -> SFuture<(Cacheable, process::Output)>
        where T: CommandCreatorSync
    {
        trace!("compile");

        let out_file = match parsed_args.outputs.get("obj") {
            Some(obj) => obj,
            None => {
                return f_err("Missing object file output")
            }
        };

        let mut attempt = creator.clone().new_command_sync(executable);
        attempt.arg("-c")
            .arg(&parsed_args.input)
            .arg("-o").arg(&out_file)
            .args(&parsed_args.preprocessor_args)
            .args(&parsed_args.common_args)
            .env_clear()
            .envs(env_vars.iter().map(|&(ref k, ref v)| (k, v)))
            .current_dir(&cwd);
        Box::new(run_input_output(attempt, None).map(|output| {
            (Cacheable::Yes, output)
        }))
    }
}

pub static ARGS: [(ArgInfo, gcc::GCCArgAttribute); 12] = [
    take_arg!("--compiler-bindir", Path, Separated, PassThrough),
    take_arg!("--compiler-options", String, Separated, PassThrough),
    take_arg!("--std", String, Separated, PassThrough),
    flag!("-O3", PassThrough),
    take_arg!("-Xcompiler", String, Separated, PassThrough),
    take_arg!("-Xfatbin", String, Separated, PassThrough),
    take_arg!("-Xptxas", String, Separated, PassThrough),
    take_arg!("-ccbin", Path, Separated, PassThrough),
    take_arg!("-gencode", String, CanBeSeparated('='), PassThrough),
    flag!("-lineinfo", PassThrough),
    take_arg!("-maxrregcount", String, CanBeSeparated('='), PassThrough),
    take_arg!("-std", String, CanBeSeparated('='), PassThrough),
];

// TODO: add some unit tests
