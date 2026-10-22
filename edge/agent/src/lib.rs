//! The TrustIoT gateway agent (decision 10): the program on the gateway that reads the field
//! devices and sends their data to the cloud.
//!
//! So far it holds [`modbus`], which reads the data points of devices over Modbus TCP or over
//! RTU-over-TCP to an RS485 converter, with one shared connection per endpoint (rebuild
//! checklist F1, F2, F5), and [`polling`], which polls each device on a fixed schedule with
//! bounded waits, reconnect backoff and a watchdog (F4). The service itself, its MQTT
//! connection and the offline buffer build on them (`README.md` lists what is where).

pub mod modbus;
pub mod polling;
