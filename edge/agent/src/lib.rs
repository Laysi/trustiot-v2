//! The TrustIoT gateway agent (decision 10): the program on the gateway that reads the field
//! devices and sends their data to the cloud.
//!
//! So far it holds [`modbus`], which reads the data points of one device over Modbus TCP
//! (rebuild checklist F1, F5), and [`tpm`], the gateway's identity key in its TPM as a rustls
//! signing key (decision 12). The service itself, its MQTT connection, scheduling and the
//! offline buffer build on them (`README.md` lists what is where).

pub mod modbus;
pub mod tpm;
