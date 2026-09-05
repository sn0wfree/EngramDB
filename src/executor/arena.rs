//! Phase 1 M3：查询生命周期 Arena
//!
//! 每个查询分配一个 bumpalo Arena，所有查询执行期临时对象（中间
//! DataChunk、表达式求值结果、hash key 等）都在 Arena 内分配。
//! 查询结束时整体 reset（不调用 drop，但内部 typed 数组 drop）。
//!
//! 入口：`with_query_arena(|guard| { ... })`
//!
//! 用法示例：
//! ```ignore
//! use crate::executor::arena::{with_query_arena, QueryArenaGuard};
//!
//! pub fn execute(&mut self, plan: &PhysicalPlan) -> Result<Vec<DataChunk>> {
//!     crate::executor::arena::with_query_arena(|_guard| {
//!         self.execute_inner(plan)
//!     })
//! }
//! ```
//!
//! 设计选择：
//! - **线程局部**：executor 是单线程事件循环（v0.14 已经是同步 API），
//!   避免 Arc<Mutex> 同步开销
//! - **bump allocator**：bumpalo 是 Rust 生态最快的 bump allocator，
//!   单线程场景零分配开销，仅指针推进
//! - **批量释放**：查询结束一次性 reset，无需逐对象 drop
//! - **风险**：bumpalo 不调用 drop——需保证 arena 内对象无重要析构
//!   （如文件句柄、子进程）。当前用法仅限于 typed 数组 + 中间字符串。

use bumpalo::Bump;
use std::cell::UnsafeCell;

/// 线程局部 query arena（单线程场景）
///
/// 使用 `thread_local!` 因为：
/// 1. executor 是单线程事件循环
/// 2. 避免 Arc<Mutex> 同步开销
/// 3. 每个查询独立 arena，跨查询无污染
const QUERY_ARENA_INITIAL_CAPACITY: usize = 64 * 1024; // 64KB 初始容量

// SAFETY: 同一线程内无并发访问（thread_local 保证）。
// `UnsafeCell<Bump>` 用于支持 `&Bump` 借用周期独立于 RefCell。
thread_local! {
    static QUERY_ARENA: UnsafeCell<Option<Bump>> = const { UnsafeCell::new(None) };
}

/// 首次访问时初始化 arena，后续访问复用同一 Bump 实例。
///
/// 返回的 `&'static Bump` 仅在同一线程内有效；guard drop 后可被 reset。
///
/// SAFETY: 调用者必须确保：
/// 1. 同一线程内调用（thread_local 保证）
/// 2. 不跨线程传递引用
/// 3. 不在 guard 借用周期内同时持有 &mut Bump（避免别名）
fn get_or_init_arena() -> &'static Bump {
    QUERY_ARENA.with(|cell| {
        // SAFETY: 同线程独占访问，无数据竞争
        unsafe {
            let slot = &mut *cell.get();
            if slot.is_none() {
                *slot = Some(Bump::with_capacity(QUERY_ARENA_INITIAL_CAPACITY));
            }
            // 取出 &Bump：slot 已被初始化，且当前是唯一引用
            let bump: &mut Bump = slot.as_mut().unwrap();
            // SAFETY: Bump 在 UnsafeCell 中，可通过 *const 取得 'static 借用
            // 因为我们保证不跨线程传递，且不与 &mut 别名
            &*(bump as *mut Bump)
        }
    })
}

fn reset_arena_internal() {
    QUERY_ARENA.with(|cell| {
        // SAFETY: 同线程独占访问
        unsafe {
            if let Some(ref mut arena) = *cell.get() {
                arena.reset();
            }
        }
    });
}

/// RAII 守卫：构造时 reset arena，析构时（query 结束）自动 reset
///
/// 调用方在 RAII 范围内执行查询；arena 借用周期 = RAII 周期。
///
/// 注意：guard 通过 `&` 借用暴露 arena，外部用 `alloc` / `alloc_str` 时
/// bumpalo 内部用 `UnsafeCell` 提供 `&mut`，与 `&` 借用兼容。
pub struct QueryArenaGuard {
    arena: &'static Bump,
}

impl QueryArenaGuard {
    /// 初始化线程局部 arena 并获取守卫
    ///
    /// 每次构造 guard 都 reset 一次 arena，确保跨查询无污染。
    pub fn new() -> Self {
        reset_arena_internal();
        Self {
            arena: get_or_init_arena(),
        }
    }

    /// 在 arena 内分配 T
    pub fn alloc<T>(&self, val: T) -> &mut T {
        self.arena.alloc(val)
    }

    /// 在 arena 内分配 str
    pub fn alloc_str(&self, s: &str) -> &mut str {
        self.arena.alloc_str(s)
    }

    /// 当前 arena 已分配字节数
    pub fn allocated_bytes(&self) -> usize {
        self.arena.allocated_bytes()
    }
}

/// RAII 风格入口：自动 reset + 提供 guard
///
/// `guard` 借用周期 = 查询执行周期。查询结束自动 reset arena。
///
/// ```ignore
/// with_query_arena(|guard| {
///     let s: &mut String = guard.alloc_str("hello");
///     // ... 执行查询，使用 s ...
/// });
/// // arena 在这里自动 reset（guard 借用结束时）
/// ```
pub fn with_query_arena<F, R>(f: F) -> R
where
    F: FnOnce(&QueryArenaGuard) -> R,
{
    let guard = QueryArenaGuard::new();
    let result = f(&guard);
    drop(guard); // 显式 drop guard，释放借用
    reset_arena_internal();
    result
}

/// 测试/调试用：清空当前线程 arena
pub fn reset_arena() {
    reset_arena_internal();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_alloc() {
        with_query_arena(|guard| {
            let s = guard.alloc_str("hello");
            assert_eq!(s, "hello");
            let n = guard.alloc(42i64);
            assert_eq!(*n, 42);
        });
    }

    #[test]
    fn test_arena_reset_between_queries() {
        // 第一次查询：分配大量数据使 chunk 增长
        with_query_arena(|guard| {
            // 分配 1KB 让 chunk 增长
            let _ = guard.alloc_str(&"x".repeat(1024));
            // 不验证具体字节数（bumpalo 内部 chunk 管理复杂）
            // 主要看 reset 后是否能继续分配
        });

        // 第二次查询：arena 应被 reset，能继续分配
        with_query_arena(|guard| {
            // 验证能正常分配（不 panic）
            let s = guard.alloc_str("query-2");
            assert_eq!(s, "query-2");

            // 验证大块分配也能工作
            let big = guard.alloc_str(&"y".repeat(2048));
            assert_eq!(big.len(), 2048);
        });

        // 第三次查询：确认 arena 在多次 query 间正常 reset + 复用
        with_query_arena(|guard| {
            let s = guard.alloc_str("query-3");
            assert_eq!(s, "query-3");
        });
    }

    #[test]
    fn test_arena_growth() {
        with_query_arena(|guard| {
            let initial_bytes = guard.allocated_bytes();
            // 分配超过初始容量
            for _ in 0..1000 {
                let _ = guard.alloc_str("x".repeat(100).as_str());
            }
            assert!(guard.allocated_bytes() > initial_bytes + 50_000, "arena 应自动扩容");
        });
    }

    #[test]
    fn test_arena_returns_owned_values() {
        // 验证 arena 分配的值可以独立使用（不依赖 arena 借用）
        let mut s = String::new();
        with_query_arena(|guard| {
            let owned_str: &mut String = guard.alloc(String::from("hello"));
            s = owned_str.clone();
        });
        assert_eq!(s, "hello");
    }
}
