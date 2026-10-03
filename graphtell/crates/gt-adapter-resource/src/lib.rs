//! Resource adapters: files that are **not source code**, turned into pipeline facts.
//!
//! These are the concrete implementations of `gt_domain::port::ResourceAdapter` — see that module for why they
//! are a port of their own and why the kernel applies their facts rather than the other way round. A resource
//! adapter knows one library's file format and nothing about the graph; the composition root registers them.

pub mod mybatis;

pub use mybatis::MyBatisMapperAdapter;
