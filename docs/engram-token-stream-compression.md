# Engram：统一 Token 流压缩算法

> 日期：2026-08-07
> 状态：设计定稿（待基准验证）
> 命名：Engram（记忆痕迹，Richard Semon 1904）——语言在存储层的「记忆痕迹」。
> 文本经统一分词成为 token 流，压缩、检索、匹配三方共享同一痕迹，如同记忆
> 的编码、提取、联想共用同一神经网络痕迹。

---

## 一、设计动机（为什么需要）

opencode 实测：流式会话事件溯源，**写放大 610×**（每次更新发布全量快照，平均 610 次/消息），
5.5GB 事件冗余无压缩。业界压缩（LZ/zstd/字典）对「近乎相同的快照序列」压缩率有限，
因为它们是**字节级**匹配，不感知语言结构。

关键洞察：**会话事件流是「逐 token 追加」的语言序列**——相邻快照共享语言前缀。
若先分词，则快照间差异 = 极少量新 token；在 **token 序列**上做前缀 delta + 词表字典，
写放大从 610× 降到 ~1×，且无损可逆。

**统一性**：分词器同时服务压缩（text）、FTS（norm）、DAG 匹配（text+offset）——
一次建设，三方复用。这是「Engram」：语言的统一记忆痕迹。

---

## 二、统一 Token 流（核心抽象）

```
原始文本
  │
  ▼
统一分词器 Tokenizer（DAG 词表驱动，确定性、可逆）
  │  TokenStream: Token { text, norm, offset }
  │
  ├──► 消费方① TokenDelta 压缩  → 消费 text（可逆源头）
  ├──► 消费方② FTS 倒排索引     → 消费 norm（检索 key，可加停用词/词形）
  └──► 消费方③ DAG 匹配/检索    → 消费 text + offset（v0.22 词图、高亮）
```

### Token 三要素

| 字段 | 含义 | 消费方 |
|---|---|---|
| `text: &str` | **原文子串**（保留原始大小写/形式）| 压缩（可逆性来源）|
| `norm: String` | 归一化形（小写等，可配置）| FTS 倒排 key |
| `offset: Range` | 原文字符区间 | DAG 匹配、高亮、重建校验 |

### 不变式
1. **确定性**：同文本 + 同词表版本 → 恒同 TokenStream
2. **可逆性**：按 offset 拼回 text 子串 → 无损还原原文（不依赖词表正确性）
3. **归一化解耦**：norm 是 text 的派生视图，永不污染压缩路径

---

## 三、分词器设计（`src/common/tokenizer.rs`）

> 定稿（2026-08-07）：数据驱动 BPE 词表 + 自研轻量零分配编码器（路线 B）
> 训练与运行时分离：**离线训练用 tokenizers（HF，Rust crate）**，
> **运行时自研编码器**（零分配、零 C 依赖、仅新增 unicode-normalization 依赖）。
> 正确性由差分测试锁定（tokenizers 编码 vs 自研编码 golden 逐 token 一致）。

### 3.1 训练端（离线，`tools/` dev-dependency: tokenizers）

```
语料（opencode 主库 + 15.6GB 备份，行级去重）
  → NFKC 归一化
  → 自定义 PreTokenizer（与运行时共享同一份规则代码）：
      类别游标切分（CJK 块 / 字母 / 数字 / 标点 / 空白 / 符号）
      + 可选种子词贪心（jieba 常用词 top-K，CJK 块内最长匹配）
  → BpeTrainer（Unicode 字符原子 + byte_fallback，v0.23.1）
  → 导出自定义词表格式（bincode）：
      magic + version + 预分割规则 + 种子词 + merges(rank 序) + 归一化标志
```

- **字符原子 + byte_fallback**（非 tiktoken 的 256 字节原子）：中文单字 = 1 token
  （字节级 = 3 token），ID 空间更小、中文压缩率更高；byte_fallback 零 OOV
- **预分割规则自研**（~80 行游标扫描，无正则依赖）：不抄 GPT-4 pat_str
  （含 lookahead，`regex` crate 不支持）；同一份规则代码在离线 bin 与运行时共享
  → 训练/运行**结构上一致**
- **三种预分割模式**（P0-2 三角对比）：纯类别切分 / +种子词 / 人工词表基准
- **BPE 编码算法**：位置最长匹配（tokenizers 同构，trie 查找）——对给定词表
  结果唯一确定，无歧义；与 tiktoken 的 rank 贪心 pair 合并不同但不相干
  （词表来自 tokenizers 训练，一致性以 tokenizers 为准）

### 3.2 运行时编码器（`src/common/tokenizer.rs`）

```
词表 include_bytes! 加载（或外部加载，版本化）
  → 类别游标预分割（共享代码）
  → 前缀 trie 位置最长匹配 → token id 序列
  → 未登录字节 fallback
  → TokenStream{ text, norm, offset }（流式零分配迭代器）
  → 静态热词映射：merges rank < TOP_N → 短 ID（TokenDelta 消费）
```

- **零分配流式输出**：压缩热路径每次 append 都编码，无 Encoding 分配开销
  （对比 tokenizers 的 `encode()` 每次分配完整 Encoding）
- **norm**：逐 token 轻量归一化（NFKC + lowercase，可配置），FTS 专用，
  永不污染 text（压缩路径）
- **offset**：预分割在原文上做，天然精确；text+offset 保证无损还原，
  不依赖词表正确性

### 3.3 差分测试（正确性锁）

- CI：离线训练产物 → tokenizers 编码 vs 自研编码，固定语料 golden
  **逐 token 一致**（id + offset）
- 覆盖：中英混排、代码、emoji、生僻字、边界空白

### 3.4 词表版本化

- 词表带 `version` 号；TokenDelta 块头记录词表版本
- 词表更新只影响**新写入块**的压缩率；旧块按自身字典 + text 解压，**永远正确**

---

## 四、TokenDelta 压缩 codec

### 4.1 两级字典

```
Token ID 空间：
  [0, TOP_N)         静态热词层：merges rank 前 N 高频 token 预分配固定短 ID（如 1024 个 → 1 字节）
  [TOP_N, ∞)         块级动态层：块内新 token 按出现顺序增量分配（块头存字典）
```

- 静态层 = 分词器词表的 merges 顺序前 N（**rank = 频率排序**，与 tiktoken
  `mergeable_ranks` 同构——选择有客观依据）
- 动态层只编码每块新 token（会话流新词率低 → 块字典极小）

### 4.2 前缀 Delta

```
事件行（一条消息的一次全量快照）：
  tokens:  [t0, t1, t2, ..., tn]（text 序列的 ID 序列）

块内编码（与块内前一个事件比较）：
  row = (shared_prefix_len, [新 token ID 序列])
  → 逐 token 追加的场景：shared_prefix_len ≈ 前事件长度，新 ID 序列极短
```

- 快照冗余 → 共享前缀：610 次更新的历史事件在块内只存各自的新增 token
- **无损**：解压 = 前事件 tokens[..shared] + 新 ID 序列 → text 按 offset 拼回

### 4.3 熵编码层（ID 流统计冗余）

前缀 delta 后，每行剩余 ID 序列的**频率分布高度倾斜**（热词占大头）——
varint 是次优编码。两级架构：**TokenDelta（结构）+ 熵编码（统计）**。

- **Huffman 起步**（自研 ~100 行，确定性、零依赖、可靠）；块头存 Huffman 表
- **rANS 候选**：若基准显示增益显著（预期 ID 流倾斜下 +10-20%），升级 rANS
  （Duda 2014，算法公开 ~150 行；Rust 生态无成熟 FSE，`rans` crate v0.4.0 小众）
- 选型由 P0-2 基准数据定夺（TokenDelta+Huffman vs TokenDelta+varint 对比）

### 4.4 编码格式（草案）

```
块头（每 Log 块）:
  [词表版本 u16][静态热词数 u16][动态字典: (text_len u32 + text) * N]
  [熵编码标志 u8（varint | Huffman | rANS）][Huffman 表（若启用）]
  [每行: shared_prefix_len varint + new_ids 熵编码流 + 行尾 offset 表]

解码：
  tokens[i] = tokens[i-1][..shared] + new_ids
  原文 = tokens 按 offset 拼接（无损）
```

### 4.5 自动选型（best-of）

`compress()` 中央分派（compression/mod.rs:32）：TokenDelta 与 Uncompressed 比较，
取体积小者（沿用现有 best-of 逻辑）。随机文本/短文本场景自动退化，零风险。

### 4.6 压缩算法分层决策（2026-08-07 定案）

| 层次 | 内容 | 状态 |
|---|---|---|
| A. 基准对照（dev-dependencies）| zstd（+ 可选纯 Rust brotli）作为字节级基线臂 | **引入**（必要，验证 TokenDelta 价值）|
| B. 运行时兜底（产品内）| 引擎内置通用压缩器（zstd/deflate）| **不引入**（由 P0-2 数据触发，见下）|
| C. 熵编码层 | 自研 Huffman 起步，rANS 候选 | 已计划 |

**运行时零 C 依赖**：自研 TokenDelta + Huffman；zstd（C 绑定）仅作 dev-dep 基准。
**触发「运行时引入兜底」的数据条件**（P0-2 出数后判定）：
- ① TokenDelta 在代码/JSON 臂压缩率 < zstd，且 ② 该类内容在 opencode 语料占比 > 30%
  → 引入兜底（优先纯 Rust miniz_oxide/deflate，零 C 依赖）
- TokenDelta 整体不敌 zstd+CDict → 直接采纳降级方案（基准工具升级为运行时依赖）

---

## 五、三方消费（统一性收益）

| 消费方 | 消费字段 | 现状 | 改造后 |
|---|---|---|---|
| ① TokenDelta 压缩 | text | Log 序列化原始字节 | 块级分词压缩 |
| ② FTS 倒排 | norm | `InvertedIndex::tokenize`（inverted_index.rs:40，CJK 整串）| 同一 Tokenizer → CJK 即刻可搜 |
| ③ DAG 匹配 | text + offset | 无 | v0.22 词图/混合检索 |

**消灭熵增**：三个功能各自维护分词逻辑/词表 → 统一为一个 Tokenizer、一份词表、一个 TokenStream。

---

## 六、业界调研（2026-08-07 复核：组件均有先例，组合无人做）

### 6.1 组件先例（我们方案的技术积木）

| 组件 | 业界对应 | 出处 |
|---|---|---|
| 前缀 delta | DELTA_BYTE_ARRAY（Incremental/front compression）| Parquet 编码 #7 |
| 列内字典 + ID | RLE_DICTIONARY | Parquet 编码 #8 |
| 静态业务字典 | zstd CDict（`zstd --train`）| zstd |
| 长度 delta | DELTA_LENGTH_BYTE_ARRAY | Parquet 编码 #6 |
| 时间序列 delta | Gorilla（delta-of-delta + XOR）| Facebook TSDB |

### 6.2 LLM Tokenizer 实战对照（2026-08-07 调研：tiktoken / SentencePiece / minbpe 官方实现）

**演进史（三阶段收敛）**：

| 阶段 | 时间 | 方法 | 代表 |
|---|---|---|---|
| 词/字符级 | 2015 前 | word-based / char-based | 早期 NMT |
| Subword 三剑客 | 2015-2018 | BPE（Sennrich, arXiv:1508.07909，频率驱动贪心合并）/ WordPiece（PMI 驱动）/ Unigram LM（概率剪枝）| 各家 |
| **字节级统一** | 2019 至今 | **BBPE**（GPT-2 发明：256 字节原子 + 零 OOV + 正则预分割）；SentencePiece（Kudo 2018，语言无关 + 空格符号化 + 自包含模型）| GPT/Llama/Mistral/Qwen 全部 |

**主流模型实际用法**：

| 模型 | 方法 | 细节 |
|---|---|---|
| GPT-2/3 | BBPE | 首创字节级 + 正则预分割 |
| GPT-4 / GPT-4o | tiktoken BBPE | cl100k(100k) / o200k(200k)，pat_str 正则预分割 |
| LLaMA 3/3.x | BBPE + regex | tiktoken 风格，128k，byte fallback |
| LLaMA 1/2、Mistral | SentencePiece BPE | 字符级 32k |
| Qwen2.x | BBPE + regex | tiktoken 风格 151k |
| Gemma 3 | SentencePiece BPE | 256k |
| Gemini | SentencePiece | 社区分析 |
| Claude | 未公开 | 计费 ~4 chars/token，暗示 BBPE 类 |
| T5/XLNet | Unigram LM | 少数派（可采样，确定性差）|
| BERT | WordPiece | 中文字级（旧体系）|

**十年收敛出的技术共识**：
1. **字节级原子兜底**（256 字节，零 OOV）——所有现代模型，无「单字兜底」
2. **数据驱动词表**：语料频率自动学习，无人工词表
3. **正则预分割**：字母/数字/标点类别边界内独立 BPE，跨类不合并（GPT-2 引入，GPT-4 沿用）
4. **确定性贪心编码（maxmatch）**：BPE 系天然确定；可采样的 Unigram 被主流抛弃
5. **自包含模型文件**（SentencePiece `.model` / tiktoken `mergeable_ranks`）
6. **性能工程**：C++/Rust 实现（SentencePiece 5 万句/秒；tiktoken 比 HF 快 3-6x，FxHashMap 等）

**中文处理**：现代 LLM 不专门处理中文——纯 BBPE 数据驱动，高频双字词由语料自然合并；
「jieba 预分词 + BPE」有先例（GNMT 中文方向、早期中文 word-level 模型）——种子词表路线有据；
但主流已放弃语言特定预分词 → **种子设计必须由 P0-2 基准三角对比定夺，不默认**。

**对 Engram 的落地修正**：
1. P0-1 采用**字节级 BPE + maxmatch + pat_str 式预分割 + 自包含词表文件**——对齐主流十年收敛路径（原「单字兜底 + 手写扫描器」计划升级；第 3 章分词器设计将随 P0-1 实施同步修订）
2. 实现直接借鉴 **minbpe**（karpathy，完整可读，MIT）+ **minbpe-rs**（Rust 移植），非从零发明
3. 预分割用 GPT-4 同款 `pat_str` 模式（minbpe regex.py 有完整实现），CJK 连续块为独立类别
4. 静态热词 = **merges 顺序前 N**：与 tiktoken `mergeable_ranks` 的 rank 排序同构——选择有客观依据
5. P0-2 基准新增**词表三角对比**：纯 BPE / 种子 BPE / jieba 预分词+BPE（压缩率 + FTS 中文命中率）

### 6.3 「token 级压缩」全被 LLM 推理路线占据（模型驱动，均不可嵌入）

| 工作 | 方法 | 结果 | 为何不可用 |
|---|---|---|---|
| **Language Modeling Is Compression**（DeepMind, arXiv:2309.10668）| Chinchilla 70B 预测 + 算术编码 | 超越 paq8 等专业压缩器 | 需 GPU 推理 |
| **LLMZip**（arXiv:2306.04050）| LLaMA-7B 预测 + 熵编码 | 压缩率超 ZPAQ/paq8h | **Llama3-8B 压 10MB 需 9.5 天** |
| **FineZip**（arXiv:2409.17141）| 在线记忆 + 动态上下文加速 | 54x 提速，压缩率 ~ 同 LLMZip | 10MB 仍 ~4 小时 |
| **AlphaZip**（arXiv:2409.15046）| transformer 预测 + 自适应 Huffman/LZ77 | 优于信息论基线 | 同上，模型级 |
| **LLM 压缩自身输出**（arXiv:2505.06297，2025）| 14 个 LLM 预测自身生成文本 | **20x+**（gzip 仅 3x）| 证实「LLM 输出高可压缩」，但需模型推理 |
| **Nacrith**（GitHub, 2026）| SmolLM2-135M + 上下文混合 + CDF 算术编码 | gzip 3.1x，超 CMIX/LLMZip/FineZip | GPU 加速仍模型级 |

### 6.4 邻近领域（语义压缩 / 存储去重，路线不同）

| 工作 | 路线 | 与我们关系 |
|---|---|---|
| **SimpleMem**（arXiv:2601.02553，3.7k★）| LLM 把对话蒸馏为原子记忆（语义有损），token 消费 -30x | 应用层语义压缩（需服务端 LLM）；我们为存储层确定性无损——互补不冲突，佐证 agent 数据压缩是热点 |
| **yams**（GitHub）| SHA-256 内容寻址 + chunk 去重 + zstd + FTS5 | 字节级块去重，无语言感知 |

### 6.5 结论（组合无先例，差异化成立）

arXiv/GitHub 检索确认：**「确定性分词器 + 词表静态字典 + 块级动态字典 + token 前缀 delta」无人做过**。
所有 token 级压缩都绑定 LLM 推理（秒~小时级/MB），嵌入引擎不可行；字节级方案不感知语言。
我们占据两者之间的空白：**语言级、确定性、µs 级、与检索共享分词器**。
2505.06297 已从理论上验证「LLM 生成文本高度可压缩」（20x 空间存在），
差异只在解法——我们用确定性分词逼近该空间，零模型依赖。

**风险仍存**：zstd 在流式追加场景的基线强度未知 → P0-2 基准先行，
若 TokenDelta 优势不显著则降级「zstd + 前缀 delta」（分词器仍建，v0.22 检索必用）。

---

## 七、基准验证计划（P0-2）

### 7.1 首轮实测结果（2026-08-07，冒烟语料 490 条 / 词表 4096）

**场景 A 流式追加（610 快照 × 3 长文本）——压缩率（越高越好）**：

| 臂 | 压缩率 | vs zstd+CDict |
|---|---|---|
| **TokenDelta+Static（词表码长）** | **562×** | **1.93×** |
| TokenDelta+Varint | 421× | 1.44× |
| TokenDelta+Huffman（块级） | 414× | 1.42× |
| zstd-3+CDict | 291× | 1.00× |
| zstd-3 | 263× | 0.90× |

**场景 B 覆盖重写（200 快照 × 3，插入型微改）**：zstd 262-289× vs TokenDelta 1.9-3.4×——**无共享前缀的随机改写场景 zstd 胜**（delta 失效 + zstd 窗口匹配强）。真实覆盖重写（相似前缀）未单独测，此类场景由 best-of 兜底规则覆盖。

**场景 C 独立文档（8 块 × 64 条）**：zstd+CDict 3.13× > TokenDelta+Static 2.82×（zstd +11%）、zstd-3 2.84×、Huffman 2.01×、Varint 1.81×。

### 7.2 数据结论（2026-08-07 定稿）

**首轮（冒烟 490 条 / 词表 4096）**：
1. 场景 A 流式：TokenDelta+Static 562× vs zstd+CDict 291×（**1.93× 优势**）——核心卖点首验成立
2. 熵编码主形态 = **Static（词表码长，L3 合并）**：三场景全面优于 Varint（+33~56%），A/C 优于块级 Huffman
3. 块级 Huffman 弃用；Varint 差距 >30% 不采用；B 类场景由兜底规则覆盖

**全量语料（71206 条 / 词表 32768，2026-08-07 补充）**：

| 臂 | 场景 A 流式 | 场景 B 重写 | 场景 C 独立文档 |
|---|---|---|---|
| TokenDelta+Varint | 513× | 1.8× | 1.8× |
| TokenDelta+Static | 732× | 2.7× | 2.6× |
| TokenDelta+Huffman | 847× | 3.8× | 2.0× |
| zstd-3+CDict | **926×** | **554×** | **2.7×** |

**全量数据反转结论（重要）**：
1. **块级批量场景 zstd 全面反超**（A 926× vs TD 847×，+9%）：zstd 的 LZ77 窗口对「块内任意位置重复」（610 行 join 后 14MB 窗口）远强于 TD 的前缀 delta——**TD 块级优势在冒烟语料被高估**（块小 + 词表小）
2. **TD 的确定性卖点 = 增量即时编码**：运行时每次事件只编码「与上一行的 delta」（KB 级），无需积压 610 事件成块；zstd 即时场景需重压全块——运行时形态：**增量 TD(Static) 为主 + zstd 块级 best-of 竞争臂**（A/B/C 全场景 zstd ≥ TD，块级由 best-of 兜底）+ Uncompressed
3. 大块场景 Huffman（块级自适应）> Static（+16%），小块 Static 胜（表头零开销）——**运行时增量小块 → Static 主形态不变**；若未来改块级批处理 → 重新评估 Huffman
4. 熵编码结论：Static 仍是增量主形态；Varint 不采用
5. 性能（全量词表）：A 场景增量编码 ~550ms/1830 事件；B/C 全量回退 ~4-5s（增量复用率 <50% 回退全量）；Static 解码 C 场景 1.6s/1113 块（每块重建 3.2 万符号表——优化项）

**词表三角（并行）**：纯 BPE / 种子 BPE（jieba 种子词）/ 人工词表基准
——同时测压缩率 + FTS 中文命中率（全量语料阶段执行）。

**决策规则（已按两轮数据更新）**：
- 运行时：增量 TokenDelta+Static 为主编码路径（事件级即时 delta）
- best-of：块级 TD vs zstd 取小；Uncompressed 兜底（占比低/无法压缩）
- 熵编码：不再实现 rANS（Static 增量下已近最优且零表头）
- 运行时兜底引入条件（见 4.6）：B 类场景（随机改写）占比高时引 zstd

---

## 八、与 v0.22 的联动

- **DAG 检索**：同一词表、同一 Tokenizer，text/offset 构建词图 → 无第二套分词
- **BM25**：norm token 频率直接来自 TokenStream
- **混合检索（RRF）**：全文与向量共享同一 token 基建
- **FTS 中文**：P0-7 即切统一 Tokenizer，v0.21 就获得中文搜索

---

## 九、验收标准

1. `Tokenizer`：同文本恒同流（确定性）、无损往返（可逆性）、CJK/英文/混合正确
2. `TokenDelta`：场景 A 压缩后 ≈ 1× 内容；场景 B < 原体积；best-of 自动退化
3. FTS 中文命中正确（MATCH）
4. Log 全局受益（任意长文本 Varchar 列）
5. 全部既有测试绿 + 新回归
