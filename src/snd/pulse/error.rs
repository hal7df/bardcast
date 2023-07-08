///! Error types for the PulseAudio driver.

extern crate libpulse_binding as libpulse;

use std::error::Error;
use std::fmt::{Display, Error as FormatError, Formatter};

use libpulse::error::{Code, PAErr};

use super::DriverComponent;

// TYPE DEFINITIONS ************************************************************

/// Error conditions that can occur during driver initialization.
#[derive(Debug, Clone)]
pub enum PulseDriverError {
    /// Error that occurred in a driver component.
    ComponentError(ComponentError),

    /// Error caused by an underlying PulseAudio failure.
    PulseError(Code),

    /// The driver failed to start due to invalid configuration, with an
    /// explanation.
    BadConfig(String),
}

#[derive(Debug, Clone)]
pub struct ComponentError(DriverComponent, Code);

// TYPE IMPLS ******************************************************************
impl ComponentError {
    pub fn new(component: DriverComponent, code: Code) -> Self {
        ComponentError(component, code)
    }
}

// TRAIT IMPLS *****************************************************************
impl From<ComponentError> for PulseDriverError {
    fn from(component_err: ComponentError) -> Self {
        PulseDriverError::ComponentError(component_err)
    }
}

impl From<Code> for PulseDriverError {
    fn from(code: Code) -> Self {
        PulseDriverError::PulseError(code)
    }
}

impl From<PAErr> for PulseDriverError {
    fn from(pa_err: PAErr) -> Self {
        PulseDriverError::from(Code::try_from(pa_err).unwrap_or(Code::Unknown))
    }
}

impl Display for PulseDriverError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        match self {
            PulseDriverError::ComponentError(e) => e.fmt(f),
            PulseDriverError::PulseError(code) => write!(f, "PulseAudio error: {}", code),
            PulseDriverError::BadConfig(reason) => write!(f, "Invalid driver configuration: {}", reason),
        }
    }
}

impl Error for PulseDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let PulseDriverError::PulseError(code) = self {
            Some(code)
        } else {
            None
        }
    }
}

impl Display for ComponentError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), FormatError> {
        write!(f, "Error in driver component '{}': {}", self.0, self.1)
    }
}

impl Error for ComponentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.1)
    }
}
