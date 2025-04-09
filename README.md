# Embedded Projects

This repository serves as a centralised collection of my IoT and embedded hardware projects. Each project contains source code, schematics, and any other resources required to recreate them.

## Technologies Used
- RP2040 microcontroller
- [ThingsBoard](https://github.com/thingsboard/thingsboard) as an IoT platform
- Embedded Rust with [Embassy](https://github.com/embassy-rs/embassy) 

## Projects

### [Humidity Monitor](./humidity-monitor/)
- Small device to measure and log humidity and temperature data, publishing to ThingsBoard for monitoring.
- Hardware: Raspberry Pico W (RP2040), SHT3x sensor