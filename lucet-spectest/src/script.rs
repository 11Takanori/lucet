use crate::bindings;
use crate::instance::Instance;
use failure::{format_err, Error, Fail};
use lucet_runtime::{self, MmapRegion, Module as LucetModule, Region, UntypedRetVal, Val};
use lucetc::{
    compile,
    compiler::OptLevel,
    error::{LucetcError, LucetcErrorKind},
    program::{HeapSettings, Program},
};
use parity_wasm::{self, deserialize_buffer};
use std::io;
use std::sync::Arc;
use std::process::Command;

#[derive(Fail, Debug)]
pub enum ScriptError {
    #[fail(display = "Deserialization error: {}", _0)]
    DeserializeError(parity_wasm::elements::Error),
    #[fail(display = "Validation error: {}", _0)]
    ValidationError(LucetcError),
    #[fail(display = "Program creation error: {}", _0)]
    ProgramError(LucetcError),
    #[fail(display = "Compilation error: {}", _0)]
    CompileError(LucetcError),
    #[fail(display = "Codegen error: {}", _0)]
    CodegenError(Error),
    #[fail(display = "Load error: {}", _0)]
    LoadError(lucet_runtime::Error),
    #[fail(display = "Instaitiation error: {}", _0)]
    InstantiateError(lucet_runtime::Error),
    #[fail(display = "Runtime error: {}", _0)]
    RuntimeError(lucet_runtime::Error),
    #[fail(display = "Malformed script: {}", _0)]
    MalformedScript(String),
    #[fail(display = "IO error: {}", _0)]
    IoError(io::Error),
}

impl ScriptError {
    pub fn unsupported(&self) -> bool {
        match self {
            ScriptError::ProgramError(ref lucetc_err)
            | ScriptError::CompileError(ref lucetc_err) => match lucetc_err.get_context() {
                &LucetcErrorKind::Unsupported(_) => true,
                _ => false,
            },
            _ => false,
        }
    }
}

impl From<io::Error> for ScriptError {
    fn from(e: io::Error) -> ScriptError {
        ScriptError::IoError(e)
    }
}

pub struct ScriptEnv {
    instances: Vec<(Option<String>, Instance)>,
}

fn program_error(e: LucetcError) -> ScriptError {
    match e.get_context() {
        LucetcErrorKind::Validation => ScriptError::ValidationError(e),
        _ => ScriptError::ProgramError(e),
    }
}

impl ScriptEnv {
    pub fn new() -> Self {
        Self {
            instances: Vec::new(),
        }
    }
    pub fn instantiate(
        &mut self,
        module: Vec<u8>,
        name: &Option<String>,
    ) -> Result<(), ScriptError> {
        let bindings = bindings::spec_test_bindings();

        let module = deserialize_buffer(&module).map_err(ScriptError::DeserializeError)?;

        let program =
            Program::new(module, bindings, HeapSettings::default()).map_err(program_error)?;

        let dir = tempfile::Builder::new().prefix("codegen").tempdir()?;
        let objfile_path = dir.path().join("a.o");
        let sofile_path = dir.path().join("a.so");

        {
            let compiler = compile(
                &program,
                &name.clone().unwrap_or("default".to_owned()),
                OptLevel::Default,
            )
            .map_err(ScriptError::CompileError)?;

            let object = compiler.codegen().map_err(ScriptError::CodegenError)?;

            object
                .write(&objfile_path)
                .map_err(ScriptError::CodegenError)?;
        }

        let mut cmd_ld = Command::new("ld");
        cmd_ld.arg(objfile_path.clone());
        cmd_ld.arg("-shared");
        cmd_ld.arg("-o");
        cmd_ld.arg(sofile_path.clone());
        let run_ld = cmd_ld.output()?;
        if !run_ld.status.success() {
            Err(ScriptError::CodegenError(format_err!(
                "ld {:?}: {}",
                objfile_path,
                String::from_utf8_lossy(&run_ld.stderr)
            )))?;
        }

        let lucet_module: Arc<dyn LucetModule> =
            lucet_runtime::DlModule::load(sofile_path).map_err(ScriptError::LoadError)?;

        let lucet_region =
            MmapRegion::create(1, &lucet_runtime::Limits::default()).expect("valid region");

        let lucet_instance = lucet_region
            .new_instance(lucet_module.clone())
            .map_err(ScriptError::InstantiateError)?;

        self.instances.push((
            name.clone(),
            Instance::new(program, lucet_module, lucet_region, lucet_instance),
        ));
        Ok(())
    }

    fn instance_named_mut(
        &mut self,
        name: &Option<String>,
    ) -> Result<&mut (Option<String>, Instance), ScriptError> {
        Ok(match name {
            // None means the last defined module should be used
            None => self
                .instances
                .last_mut()
                .ok_or_else(|| ScriptError::MalformedScript("no defined instances".to_owned()))?,
            Some(ref n) => self
                .instances
                .iter_mut()
                .find(|(iname, _)| *iname == *name)
                .ok_or_else(|| ScriptError::MalformedScript(format!("no instance named {}", n)))?,
        })
    }

    pub fn instance_named(&self, name: &Option<String>) -> Result<&Instance, ScriptError> {
        Ok(match name {
            // None means the last defined module should be used
            None => self
                .instances
                .last()
                .map(|(_fst, snd)| snd)
                .ok_or_else(|| ScriptError::MalformedScript("no defined instances".to_owned()))?,
            Some(ref n) => self
                .instances
                .iter()
                .find(|(iname, _)| *iname == *name)
                .map(|(_fst, snd)| snd)
                .ok_or_else(|| ScriptError::MalformedScript(format!("no instance named {}", n)))?,
        })
    }

    pub fn run(
        &mut self,
        name: &Option<String>,
        field: &str,
        args: Vec<Val>,
    ) -> Result<UntypedRetVal, ScriptError> {
        let (_, ref mut inst) = self.instance_named_mut(name)?;
        inst.run(&field, &args)
            .map_err(|e| ScriptError::RuntimeError(e))
    }

    pub fn register(&mut self, name: &Option<String>, as_name: &str) -> Result<(), ScriptError> {
        let (ref mut oldname, _) = self.instance_named_mut(name)?;
        *oldname = Some(as_name.to_owned());
        Ok(())
    }

    pub fn delete_last(&mut self) {
        let last_index = self.instances.len() - 1;
        self.instances.remove(last_index);
    }
}
