pub mod bindings;
pub mod compiler;
pub mod error;
pub mod load;
pub mod new;
pub mod patch;
pub mod program;

use crate::compiler::function::compile_function;
use crate::compiler::module_data::compile_module_data;
use crate::compiler::table::compile_table;
use crate::error::{LucetcError, LucetcErrorKind};
use crate::load::read_module;
use crate::patch::patch_module;
use crate::program::Program;
use failure::{format_err, Error, ResultExt};
use parity_wasm::elements::Module;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tempfile;

pub use crate::{
    bindings::Bindings,
    compiler::{Compiler, OptLevel},
    program::memory::HeapSettings,
};

pub struct Lucetc {
    module: Module,
    name: String,
    bindings: Bindings,
    opt_level: OptLevel,
    heap: HeapSettings,
    builtins_path: Option<PathBuf>,
}

impl Lucetc {
    pub fn new<P: AsRef<Path>>(input: P) -> Result<Self, LucetcError> {
        let input = input.as_ref();
        let module = read_module(input)?;
        let name = String::from(
            input
                .file_stem()
                .ok_or(format_err!("input filename {:?} is not a file", input))?
                .to_str()
                .ok_or(format_err!("input filename {:?} is not valid utf8", input))?,
        );
        Ok(Self {
            module,
            name,
            bindings: Bindings::empty(),
            opt_level: OptLevel::default(),
            heap: HeapSettings::default(),
            builtins_path: None,
        })
    }

    pub fn bindings(mut self, bindings: Bindings) -> Result<Self, Error> {
        self.with_bindings(bindings)?;
        Ok(self)
    }
    pub fn with_bindings(&mut self, bindings: Bindings) -> Result<(), Error> {
        self.bindings.extend(bindings)
    }

    pub fn opt_level(mut self, opt_level: OptLevel) -> Self {
        self.with_opt_level(opt_level);
        self
    }
    pub fn with_opt_level(&mut self, opt_level: OptLevel) {
        self.opt_level = opt_level;
    }

    pub fn builtins<P: AsRef<Path>>(mut self, builtins: P) -> Result<Self, Error> {
        self.with_builtins(builtins)?;
        Ok(self)
    }
    pub fn with_builtins<P: AsRef<Path>>(&mut self, builtins_path: P) -> Result<(), Error> {
        let (newmodule, builtins_map) = patch_module(self.module.clone(), builtins_path)?;
        self.module = newmodule;
        self.bindings.extend(Bindings::env(builtins_map))?;
        Ok(())
    }

    pub fn reserved_size(mut self, reserved_size: u64) -> Self {
        self.with_reserved_size(reserved_size);
        self
    }
    pub fn with_reserved_size(&mut self, reserved_size: u64) {
        self.heap.reserved_size = reserved_size;
    }

    pub fn guard_size(mut self, guard_size: u64) -> Self {
        self.with_guard_size(guard_size);
        self
    }
    pub fn with_guard_size(&mut self, guard_size: u64) {
        self.heap.guard_size = guard_size;
    }

    pub fn object_file<P: AsRef<Path>>(self, output: P) -> Result<(), Error> {
        let prog = Program::new(self.module, self.bindings, self.heap.clone())?;
        let comp = compile(&prog, &self.name, self.opt_level)?;

        let obj = comp.codegen()?;
        obj.write(output.as_ref()).context("writing object file")?;

        Ok(())
    }

    pub fn clif_ir<P: AsRef<Path>>(self, output: P) -> Result<(), Error> {
        let (module, builtins_map) = if let Some(ref builtins_path) = self.builtins_path {
            patch_module(self.module, builtins_path)?
        } else {
            (self.module, HashMap::new())
        };

        let mut bindings = self.bindings.clone();
        bindings.extend(Bindings::env(builtins_map))?;

        let prog = Program::new(module, bindings, self.heap.clone())?;
        let comp = compile(&prog, &self.name, self.opt_level)?;

        comp.cranelift_funcs()
            .write(&output)
            .context("writing clif file")?;

        Ok(())
    }

    pub fn shared_object_file<P: AsRef<Path>>(self, output: P) -> Result<(), Error> {
        let dir = tempfile::Builder::new().prefix("lucetc").tempdir()?;
        let objpath = dir.path().join("tmp.o");
        self.object_file(objpath.clone())?;
        link_so(objpath, output)?;
        Ok(())
    }
}

fn link_so<P, Q>(objpath: P, sopath: Q) -> Result<(), Error>
where
    P: AsRef<Path>,
    Q: AsRef<Path>,
{
    use std::process::Command;
    let mut cmd_ld = Command::new("ld");
    cmd_ld.arg(objpath.as_ref());
    cmd_ld.arg("-shared");
    cmd_ld.arg("-o");
    cmd_ld.arg(sopath.as_ref());

    let run_ld = cmd_ld
        .output()
        .context(format_err!("running ld on {:?}", objpath.as_ref()))?;

    if !run_ld.status.success() {
        Err(format_err!(
            "ld of {} failed: {}",
            objpath.as_ref().to_str().unwrap(),
            String::from_utf8_lossy(&run_ld.stderr)
        ))?;
    }
    Ok(())
}

pub fn compile<'p>(
    program: &'p Program,
    name: &str,
    opt_level: OptLevel,
) -> Result<Compiler<'p>, LucetcError> {
    let mut compiler = Compiler::new(name.to_owned(), &program, opt_level)?;

    compile_module_data(&mut compiler).context(LucetcErrorKind::ModuleData)?;

    for function in program.defined_functions() {
        let body = program.function_body(&function);
        compile_function(&mut compiler, &function, body)
            .context(LucetcErrorKind::Function(function.symbol().to_owned()))?;
    }
    for table in program.tables() {
        compile_table(&mut compiler, &table)
            .context(LucetcErrorKind::Table(table.symbol().to_owned()))?;
    }

    Ok(compiler)
}
