# HybridDB 功能补齐路线图（查缺补漏版）

> 基准版本：v0\.12\.0
> 对标基线：SQLite 3\.45 功能集 \+ AI Agent 场景特有需求
> 规划日期：2026\-08\-03
> 
> 

---

## 一、功能全景清单

### 1\.1 数据类型（21 项）

|\#|类型|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|T01|INT / INTEGER|✅ 已有|—|—|64位有符号整数|
|T02|BIGINT / INT64|✅ 已有|—|—|同 INTEGER|
|T03|TINYINT / INT8|🟡 别名未支持|P2|v0\.14|1字节整数，需在类型映射中添加|
|T04|SMALLINT / INT16|🟡 别名未支持|P2|v0\.14|2字节整数|
|T05|INT32 / MEDIUMINT|✅ 已有|—|—|4字节整数（已在 speedtest 修复）|
|T06|FLOAT / DOUBLE / REAL|✅ 已有|—|—|64位浮点|
|T07|FLOAT32|🔴 缺失|P2|v0\.15|32位浮点，节省存储|
|T08|BOOLEAN|✅ 已有|—|—|底层用 INTEGER 0/1|
|T09|VARCHAR / TEXT / CHAR|✅ 已有|—|—|变长字符串|
|T10|BLOB / BINARY|🔴 缺失|P1|v0\.14|二进制大对象，Agent 存原始数据|
|T11|DATE|🔴 缺失|P2|v0\.16|日期类型（YYYY\-MM\-DD）|
|T12|TIME|🔴 缺失|P3|v0\.17\+|时间类型（HH:MM:SS）|
|T13|DATETIME / TIMESTAMP|🔴 缺失|P1|v0\.15|日期时间，Agent 日志/记忆刚需|
|T14|DECIMAL / NUMERIC|🔴 缺失|P3|v0\.17\+|精确十进制，金融场景|
|T15|JSON|🔴 缺失|P0|v0\.14|JSON 类型 \+ 路径查询，Agent 元数据刚需|
|T16|JSONB|🔴 缺失|P2|v0\.16|二进制 JSON，解析更快|
|T17|UUID|🔴 缺失|P3|v0\.17\+|UUID 类型，分布式 ID|
|T18|ENUM / SET|🔴 缺失|P3|v0\.17\+|枚举类型|
|T19|VECTOR\(dim\)|✅ 已有|—|—|向量类型（HNSW 索引）|
|T20|VECTOR\_INT8|🔴 缺失|P1|v0\.15|INT8 量化向量，存储减 75%|
|T21|ARRAY|🔴 缺失|P2|v0\.16|数组类型|

**小计**：已有 5 项 \+ 部分 1 项 \+ 缺失 15 项

---

### 1\.2 DDL 语句（18 项）

|\#|语句|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|D01|CREATE TABLE|✅ 已有|—|—|基础建表|
|D02|CREATE TABLE \.\.\. AS SELECT|🔴 缺失|P2|v0\.15|CTAS，从查询结果建表|
|D03|CREATE TABLE IF NOT EXISTS|✅ 已有|—|—|幂等建表|
|D04|DROP TABLE|✅ 已有|—|—|删表|
|D05|DROP TABLE IF EXISTS|✅ 已有|—|—|幂等删表|
|D06|ALTER TABLE ADD COLUMN|🔴 缺失|P0|v0\.14|加列，Schema 演进刚需|
|D07|ALTER TABLE DROP COLUMN|🔴 缺失|P2|v0\.16|删列|
|D08|ALTER TABLE RENAME COLUMN|🔴 缺失|P2|v0\.16|改列名|
|D09|ALTER TABLE RENAME TO|🔴 缺失|P2|v0\.16|改表名|
|D10|ALTER TABLE ALTER COLUMN|🔴 缺失|P3|v0\.17\+|改列类型|
|D11|CREATE INDEX|✅ 已有（部分）|P0|v0\.13|当前只有 SkipList，需补 B\+Tree|
|D12|CREATE UNIQUE INDEX|🔴 缺失|P1|v0\.14|唯一索引|
|D13|CREATE INDEX \.\.\. WHERE|🔴 缺失|P3|v0\.17\+|部分索引|
|D14|DROP INDEX|✅ 已有|—|—|删索引|
|D15|CREATE VIEW / DROP VIEW|🔴 缺失|P3|v0\.17\+|视图|
|D16|TRUNCATE TABLE|🔴 缺失|P2|v0\.15|清空表（比 DELETE 快）|
|D17|VACUUM / COMPACT|✅ 已有|—|—|LSM 合并 / 空间回收|
|D18|ANALYZE|🔴 缺失|P1|v0\.16|收集统计信息，CBO 基础|

**小计**：已有 6 项 \+ 部分 1 项 \+ 缺失 11 项

---

### 1\.3 约束（7 项）

|\#|约束|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|C01|PRIMARY KEY|✅ 已有|—|—|主键约束|
|C02|AUTO\_INCREMENT / SERIAL|🔴 缺失|P0|v0\.14|自增主键，应用开发刚需|
|C03|NOT NULL|🔴 缺失|P1|v0\.14|非空约束|
|C04|UNIQUE|🔴 缺失|P1|v0\.14|唯一约束（Token key 等）|
|C05|FOREIGN KEY \+ CASCADE|🔴 缺失|P1|v0\.14|外键 \+ 级联删除/更新|
|C06|CHECK|🔴 缺失|P3|v0\.17\+|CHECK 约束|
|C07|DEFAULT 值|🔴 缺失|P1|v0\.14|列默认值|

**小计**：已有 1 项 \+ 缺失 6 项

---

### 1\.4 DML 语句（14 项）

|\#|语句|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|M01|INSERT VALUES|✅ 已有|—|—|单行插入|
|M02|INSERT 多行 \(VALUES \(\.\.\.\), \(\.\.\.\)\)|🟡 部分支持|P0|v0\.13|批量插入，性能关键|
|M03|INSERT \.\.\. SELECT|🔴 缺失|P2|v0\.15|从查询结果插入|
|M04|INSERT \.\.\. RETURNING|🔴 缺失|P0|v0\.14|返回生成的 ID|
|M05|INSERT OR IGNORE / REPLACE / ROLLBACK|🔴 缺失|P2|v0\.15|冲突解决策略|
|M06|UPSERT \(ON CONFLICT DO UPDATE\)|🔴 缺失|P0|v0\.14|幂等更新，缓存/计数器刚需|
|M07|SELECT|✅ 已有|—|—|基础查询|
|M08|UPDATE|✅ 已有|—|—|更新|
|M09|UPDATE \.\.\. FROM|🔴 缺失|P3|v0\.17\+|关联更新|
|M10|DELETE|✅ 已有|—|—|删除|
|M11|DELETE \.\.\. USING|🔴 缺失|P3|v0\.17\+|关联删除|
|M12|MERGE / UPSERT 扩展|🔴 缺失|P3|v0\.17\+|MERGE INTO|
|M13|COPY / BULK COPY|🔴 缺失|P1|v0\.14|批量导入导出|
|M14|REPLACE|🔴 缺失|P2|v0\.15|替换插入|

**小计**：已有 4 项 \+ 部分 1 项 \+ 缺失 9 项

---

### 1\.5 查询功能（30 项）

|\#|功能|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|Q01|WHERE 条件|✅ 已有|—|—|基础过滤|
|Q02|AND / OR / NOT|✅ 已有|—|—|逻辑运算符|
|Q03|= / \!= / \< / \> / \<= / \>=|✅ 已有|—|—|比较运算符|
|Q04|IN \(列表\)|🔴 缺失|P1|v0\.14|IN 子句，多值匹配|
|Q05|BETWEEN \.\.\. AND|🔴 缺失|P1|v0\.14|范围查询语法糖|
|Q06|LIKE 模糊匹配|🔴 缺失|P1|v0\.15|字符串模糊搜索|
|Q07|GLOB|🔴 缺失|P3|v0\.17\+|通配符匹配|
|Q08|REGEXP|🔴 缺失|P2|v0\.16|正则匹配|
|Q09|IS NULL / IS NOT NULL|🔴 缺失|P1|v0\.14|NULL 判断|
|Q10|IS / IS NOT|🔴 缺失|P2|v0\.15|通用比较（含布尔）|
|Q11|DISTINCT|🔴 缺失|P1|v0\.14|去重|
|Q12|ORDER BY|✅ 已有|—|—|排序|
|Q13|ORDER BY 多列|🟡 部分|P1|v0\.14|多列排序|
|Q14|ORDER BY DESC / ASC|✅ 已有|—|—|升降序|
|Q15|LIMIT / OFFSET|✅ 已有|—|—|分页|
|Q16|GROUP BY|✅ 已有|—|—|分组|
|Q17|GROUP BY 多列|🟡 部分|P1|v0\.14|多列分组|
|Q18|HAVING|🔴 缺失|P2|v0\.15|分组后过滤|
|Q19|INNER JOIN|🔴 缺失|P1|v0\.16|内连接|
|Q20|LEFT / RIGHT OUTER JOIN|🔴 缺失|P1|v0\.16|外连接|
|Q21|CROSS JOIN|🔴 缺失|P2|v0\.16|笛卡尔积|
|Q22|FULL OUTER JOIN|🔴 缺失|P2|v0\.17\+|全外连接|
|Q23|子查询 \(IN / EXISTS\)|🔴 缺失|P1|v0\.15|子查询|
|Q24|标量子查询|🔴 缺失|P2|v0\.16|SELECT 列中的子查询|
|Q25|CTE \(WITH \.\.\. AS\)|🔴 缺失|P2|v0\.16|公用表表达式|
|Q26|递归 CTE|🔴 缺失|P3|v0\.17\+|递归查询|
|Q27|UNION / UNION ALL|🔴 缺失|P1|v0\.15|合并查询结果|
|Q28|INTERSECT / EXCEPT|🔴 缺失|P2|v0\.16|交集/差集|
|Q29|CASE WHEN \.\.\. THEN \.\.\. ELSE \.\.\. END|🔴 缺失|P1|v0\.15|条件表达式|
|Q30|SELECT \* EXCEPT / REPLACE|🔴 缺失|P3|v0\.17\+|列排除/替换|

**小计**：已有 7 项 \+ 部分 3 项 \+ 缺失 20 项

---

### 1\.6 聚合函数（14 项）

|\#|函数|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|A01|COUNT\(\*\)|✅ 已有|—|—|计数|
|A02|COUNT\(col\) / COUNT\(DISTINCT col\)|🟡 部分|P1|v0\.14|列计数 / 去重计数|
|A03|SUM|✅ 已有|—|—|求和|
|A04|AVG|✅ 已有|—|—|平均|
|A05|MIN|✅ 已有|—|—|最小|
|A06|MAX|✅ 已有|—|—|最大|
|A07|GROUP\_CONCAT|🔴 缺失|P2|v0\.16|字符串拼接聚合|
|A08|STDDEV / VARIANCE|🔴 缺失|P2|v0\.16|标准差/方差|
|A09|PERCENTILE / MEDIAN|🔴 缺失|P2|v0\.16|百分位/中位数|
|A10|FIRST\_VALUE / LAST\_VALUE|🔴 缺失|P2|v0\.16|首尾值|
|A11|窗口函数 \(ROW\_NUMBER / RANK / DENSE\_RANK\)|🔴 缺失|P2|v0\.16|排名窗口函数|
|A12|窗口函数 \(LAG / LEAD\)|🔴 缺失|P2|v0\.16|偏移窗口函数|
|A13|窗口函数 \(SUM/AVG OVER\)|🔴 缺失|P2|v0\.16|聚合窗口函数|
|A14|NTILE / PERCENT\_RANK / CUME\_DIST|🔴 缺失|P3|v0\.17\+|分布窗口函数|

**小计**：已有 5 项 \+ 部分 1 项 \+ 缺失 8 项

---

### 1\.7 标量函数（40 项）

#### 字符串函数（15 项）

|\#|函数|优先级|归属版本|说明|
|---|---|---|---|---|
|S01|LENGTH / CHAR\_LENGTH|P1|v0\.14|字符串长度|
|S02|SUBSTR / SUBSTRING|P1|v0\.14|截取子串|
|S03|CONCAT /|||P1|
|S04|UPPER / LOWER|P1|v0\.14|大小写转换|
|S05|TRIM / LTRIM / RTRIM|P2|v0\.15|去除空白|
|S06|REPLACE|P1|v0\.14|替换|
|S07|INSTR / POSITION|P2|v0\.15|查找子串位置|
|S08|LPAD / RPAD|P3|v0\.17\+|填充|
|S09|REVERSE|P3|v0\.17\+|反转|
|S10|REPEAT|P3|v0\.17\+|重复|
|S11|SPLIT\_PART|P2|v0\.16|分割取第N段|
|S12|FORMAT / PRINTF|P3|v0\.17\+|格式化|
|S13|HEX / CHAR|P3|v0\.17\+|十六进制/字符转换|
|S14|SOUNDEX|P3|v0\.17\+|语音编码|
|S15|UNICODE|P3|v0\.17\+|Unicode 码点|

#### 数值函数（12 项）

|\#|函数|优先级|归属版本|说明|
|---|---|---|---|---|
|N01|ABS|P1|v0\.14|绝对值|
|N02|ROUND|P1|v0\.14|四舍五入|
|N03|CEIL / CEILING|P2|v0\.15|向上取整|
|N04|FLOOR|P2|v0\.15|向下取整|
|N05|TRUNC|P2|v0\.15|截断|
|N06|MOD / %|P1|v0\.14|取模|
|N07|POWER / POW|P2|v0\.15|幂运算|
|N08|SQRT|P2|v0\.15|平方根|
|N09|LN / LOG / LOG10 / LOG2|P2|v0\.16|对数|
|N10|EXP|P3|v0\.17\+|指数|
|N11|PI / RADIANS / DEGREES|P3|v0\.17\+|三角函数常量|
|N12|RANDOM|P2|v0\.16|随机数|

#### 日期时间函数（8 项）

|\#|函数|优先级|归属版本|说明|
|---|---|---|---|---|
|Dt01|NOW / CURRENT\_TIMESTAMP|P1|v0\.15|当前时间|
|Dt02|DATE / TIME / DATETIME|P1|v0\.15|日期/时间提取|
|Dt03|STRFTIME|P1|v0\.15|格式化时间|
|Dt04|DATE\_TRUNC / date\_bin|P1|v0\.16|时间桶截断（日志分析刚需）|
|Dt05|JULIANDAY|P3|v0\.17\+|儒略日|
|Dt06|日期加减 \(DATE\_ADD / DATE\_SUB\)|P1|v0\.15|日期运算|
|Dt07|日期差 \(DATEDIFF / JULIANDAY 差\)|P2|v0\.16|日期间隔计算|
|Dt08|STRPTIME|P2|v0\.16|字符串转时间|

#### 条件/类型转换函数（5 项）

|\#|函数|优先级|归属版本|说明|
|---|---|---|---|---|
|F01|CAST\(expr AS type\)|P0|v0\.14|类型转换，SQL 标准|
|F02|COALESCE / IFNULL|P1|v0\.14|取第一个非 NULL 值|
|F03|NULLIF|P2|v0\.15|相等返回 NULL|
|F04|IF\(cond, true\_val, false\_val\)|P2|v0\.15|三元条件|
|F05|TYPEOF|P2|v0\.16|返回值的类型|

**小计**：已有 0 项 \+ 缺失 40 项

---

### 1\.8 JSON 函数（12 项）

|\#|函数|优先级|归属版本|说明|
|---|---|---|---|---|
|J01|json\_extract\(col, path\)|P0|v0\.14|提取 JSON 字段值|
|J02|col \-\> 'key' / col \-\>\> 'key'|P0|v0\.14|路径操作符（PG 风格）|
|J03|json\_object\(k1,v1,k2,v2,\.\.\.\)|P1|v0\.15|构造 JSON 对象|
|J04|json\_array\(v1,v2,\.\.\.\)|P1|v0\.15|构造 JSON 数组|
|J05|json\_each / json\_tree|P2|v0\.16|JSON 展开为行集|
|J06|json\_set / json\_insert / json\_replace|P1|v0\.15|修改 JSON|
|J07|json\_remove|P2|v0\.16|删除 JSON 字段|
|J08|json\_array\_length|P1|v0\.15|数组长度|
|J09|json\_type / json\_valid|P2|v0\.16|JSON 类型/校验|
|J10|json\_agg|P2|v0\.16|聚合为 JSON 数组|
|J11|json\_group\_object|P2|v0\.16|聚合为 JSON 对象|
|J12|jsonb 二进制存储|P2|v0\.16|二进制 JSON，解析更快|

**小计**：已有 0 项 \+ 缺失 12 项

---

### 1\.9 索引（9 项）

|\#|索引类型|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|I01|SkipList 跳表索引|✅ 已有|—|—|当前默认二级索引|
|I02|B\+Tree 主键索引|🔴 缺失|P0|v0\.13|点查性能关键|
|I03|B\+Tree 二级索引|🔴 缺失|P1|v0\.14|通用二级索引|
|I04|唯一索引 \(UNIQUE\)|🔴 缺失|P1|v0\.14|唯一约束|
|I05|复合索引 \(多列\)|🔴 缺失|P1|v0\.14|多列索引|
|I06|覆盖索引|✅ 已有|—|—|索引直接返回列值|
|I07|位图索引|✅ 已有|—|—|低基数列优化|
|I08|布隆过滤器索引|✅ 已有|—|—|存在性判断加速|
|I09|全文索引 \(FTS5\)|🔴 缺失|P2|v0\.17|全文检索，Agent 日志搜索|
|I10|表达式索引|🔴 缺失|P3|v0\.17\+|基于表达式的索引|
|I11|部分索引 \(WHERE\)|🔴 缺失|P3|v0\.17\+|条件索引|
|I12|REINDEX|🔴 缺失|P2|v0\.16|重建索引|

**小计**：已有 4 项 \+ 缺失 8 项

---

### 1\.10 向量检索（8 项）

|\#|功能|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|V01|HNSW 索引构建|✅ 已有|—|—|M/ef 参数可调|
|V02|L2 / 内积 / 余弦距离|✅ 已有|—|—|三种距离度量|
|V03|Top\-K 搜索|✅ 已有|—|—|KNN 查询|
|V04|Tombstone 删除维护|✅ 已有|—|—|删除标记 \+ 自动补偿|
|V05|索引持久化|✅ 已有|—|—|存盘/恢复|
|V06|增量插入|✅ 已有|—|—|增量添加向量|
|V07|混合查询（向量 \+ 标量过滤）|🔴 缺失|P0|v0\.15|Agent 记忆检索刚需|
|V08|INT8 向量量化|🔴 缺失|P1|v0\.15|存储减 75%，边缘场景|
|V09|IVF 倒排索引|🔴 缺失|P2|v0\.17\+|亿级向量场景|
|V10|PQ 乘积量化|🔴 缺失|P2|v0\.17\+|高压缩比|
|V11|多向量字段|🔴 缺失|P2|v0\.16|多列向量索引|
|V12|向量索引并行构建|🔴 缺失|P3|v0\.17\+|多核加速构建|

**小计**：已有 6 项 \+ 缺失 6 项

---

### 1\.11 事务与并发（10 项）

|\#|功能|v0\.12 状态|优先级|归属版本|说明|
|---|---|---|---|---|---|
|Txn01|BEGIN / COMMIT / ROLLBACK|✅ 已有|—|—|基础事务|
|Txn02|WAL 预写日志|✅ 已有|—|—|持久化|
|Txn03|MVCC 快照隔离|✅ 已有|—|—|读写不阻塞|
|Txn04|WAL 组提交|🔴 缺失|P0|v0\.13|写入吞吐 5\-10× 提升|
|Txn05|SAVEPOINT|🔴 缺失|P2|v0\.15|保存点，部分回滚|
|Txn06|事务隔离级别设置|🔴 缺失|P2|v0\.16|READ UNCOMMITTED / SNAPSHOT 等|
|Txn07|死锁检测|🔴 缺失|P2|v0\.16|写写冲突检测优化|
|Txn08|WAL checkpoint 控制|🔴 缺失|P2|v0\.15|WAL 检查点管理|
|Txn09|只读事务优化|🔴 缺失|P2|v0\.15|只读事务跳过 WAL|
|Txn10|两阶段提交 \(2PC\)|🔴 缺失|P3|v0\.17\+|分布式事务|

**小计**：已有 3 项 \+ 缺失 7 项

---

### 1\.12 系统表与 PRAGMA（12 项）

|\#|功能|优先级|归属版本|说明|
|---|---|---|---|---|
|P01|information\_schema\.tables / columns|P1|v0\.14|元数据查询|
|P02|PRAGMA table\_info|P1|v0\.14|表结构查询|
|P03|PRAGMA index\_info / index\_list|P2|v0\.15|索引信息|
|P04|PRAGMA journal\_mode|P2|v0\.15|WAL / 普通模式切换|
|P05|PRAGMA synchronous|P2|v0\.15|同步级别设置|
|P06|PRAGMA cache\_size|P2|v0\.15|缓存大小设置|
|P07|PRAGMA page\_size / page\_count|P2|v0\.16|页大小/页数|
|P08|PRAGMA foreign\_keys|P1|v0\.14|外键开关|
|P09|PRAGMA integrity\_check|P3|v0\.17\+|数据完整性校验|
|P10|PRAGMA optimize|P3|v0\.17\+|自动优化|
|P11|EXPLAIN / EXPLAIN ANALYZE|P1|v0\.15|查询计划查看|
|P12|统计信息表 \(sqlite\_stat1/3/4\)|P2|v0\.16|ANALYZE 产物|

**小计**：已有 0 项 \+ 缺失 12 项

---

### 1\.13 Agent 场景特有（10 项）

|\#|功能|优先级|归属版本|说明|
|---|---|---|---|---|
|Ag01|内置 TTL（表级）|P0|v0\.15|日志/短期记忆自动过期|
|Ag02|内置 KV 缓存引擎|P1|v0\.15|LRU \+ TTL \+ hit\_count，替代应用层缓存|
|Ag03|滑动窗口限流原语|P1|v0\.15|网关 RPM/TPM 限流、熔断计数|
|Ag04|分层记忆接口（短期/长期/工作）|P2|v0\.16|Agent 记忆分层管理|
|Ag05|记忆重要性评分 \+ 衰减|P2|v0\.17|自动评估记忆重要性，时间衰减|
|Ag06|全文 \+ 向量混合检索|P2|v0\.17|BM25 \+ 向量相似度联合排序|
|Ag07|对话历史时序优化|P1|v0\.15|按会话\+时间组织，时序查询优化|
|Ag08|工具调用日志专用表|P2|v0\.16|Agent 工具调用记录与分析|
|Ag09|任务状态机（工作流）|P3|v0\.17\+|任务/步骤状态流转|
|Ag10|多模态 Blob 列存|P3|v0\.17\+|图像/音频等原始数据列存优化|

**小计**：已有 0 项 \+ 缺失 10 项

---

### 1\.14 性能优化（10 项）

|\#|优化项|优先级|归属版本|说明|
|---|---|---|---|---|
|Perf01|行数元数据缓存|P0|v0\.13|COUNT\(\*\) 从全扫 → O\(1\)|
|Perf02|Prepared Statement / 查询计划缓存|P0|v0\.14|高频查询跳过解析/规划|
|Perf03|B\+Tree 主键索引（点查优化）|P0|v0\.13|点查 34× → 接近 SQLite|
|Perf04|WAL 组提交|P0|v0\.13|事务写入 5\-10×|
|Perf05|Top\-N 排序优化（堆排序）|P1|v0\.14|ORDER BY \+ LIMIT 避免全排序|
|Perf06|向量化 JOIN（Hash Join）|P1|v0\.16|JOIN 性能提升|
|Perf07|向量化表达式计算|P1|v0\.15|WHERE/HAVING 表达式向量化|
|Perf08|CBO 基于代价的优化器|P2|v0\.16|自动选最优执行计划|
|Perf09|多线程并行查询|P2|v0\.17|多核加速大查询|
|Perf10|向量化窗口函数|P2|v0\.16|窗口函数向量化执行|

**小计**：已有 0 项 \+ 缺失 10 项

---

### 1\.15 接口与生态（8 项）

|\#|功能|优先级|归属版本|说明|
|---|---|---|---|---|
|Eco01|Rust 高级 API \(Connection/Transaction\)|P0|v0\.13|完善 Rust 接口，当前 API 较底层|
|Eco02|C API \(libhybriddb\)|P2|v0\.17|C 接口，便于多语言绑定|
|Eco03|Python 绑定 \(pyo3\)|P1|v0\.16|Python 生态，对接 LangChain|
|Eco04|命令行 CLI \(hybridcli\)|P1|v0\.15|交互式查询工具|
|Eco05|导出 / 导入 \(CSV / JSON / Parquet\)|P2|v0\.16|数据迁移|
|Eco06|SQLite 兼容模式|P2|v0\.17|降低迁移成本|
|Eco07|ATTACH DATABASE|P2|v0\.16|跨库查询|
|Eco08|备份与恢复 API|P2|v0\.16|在线备份|

**小计**：已有 0 项 \+ 缺失 8 项

---

## 二、功能统计汇总

|类别|总数|已有|部分|缺失|缺失率|
|---|---|---|---|---|---|
|数据类型|21|5|1|15|71%|
|DDL 语句|18|6|1|11|61%|
|约束|7|1|0|6|86%|
|DML 语句|14|4|1|9|64%|
|查询功能|30|7|3|20|67%|
|聚合函数|14|5|1|8|57%|
|标量函数|40|0|0|40|100%|
|JSON 函数|12|0|0|12|100%|
|索引|12|4|0|8|67%|
|向量检索|12|6|0|6|50%|
|事务与并发|10|3|0|7|70%|
|系统表/PRAGMA|12|0|0|12|100%|
|Agent 特有|10|0|0|10|100%|
|性能优化|10|0|0|10|100%|
|接口与生态|8|0|0|8|100%|
|**合计**|**230**|**41**|**7**|**182**|**79%**|

> 当前功能完成度约 21%（按项数计）。核心骨架（存储引擎 \+ 事务 \+ 向量 \+ 基础 SQL）已到位，但上层 SQL 功能和生态仍有大量补齐空间。
> 
> 

---

## 三、分版本详细排期

### v0\.13 — 性能攻坚（2\-3 周）

**主题**：解决最痛的性能短板，OLTP 场景显著提速

**核心目标**：

- 事务写入吞吐提升 5\-10×

- 索引点查性能提升 10×\+

- COUNT\(\*\) 从 12ms → \<10µs

|编号|功能点|工作量|依赖|
|---|---|---|---|
|Perf01|行数元数据缓存|小|无|
|Perf03|B\+Tree 主键索引|大|无|
|Perf04|WAL 组提交|中|无|
|M02|INSERT 多行批量执行优化|中|无|
|Eco01|Rust 高级 API 完善|中|无|
|I02|B\+Tree 二级索引（第一阶段）|中|I02|

**验收标准**：

- 事务内逐行写入：49× 慢 → ≤ 10× 慢（vs SQLite）

- 索引点查：34× 慢 → ≤ 5× 慢

- COUNT\(\*\)：14× 慢 → 持平或更快

---

### v0\.14 — SQL 基础补齐（3\-4 周）

**主题**：达到可替换 SQLite 基础场景的门槛

**核心目标**：

- 自增 \+ UPSERT \+ JSON 三大刚需落地

- 基础 SQL 功能覆盖 60%\+

- llmRx Store 接口 60\+ 方法可跑通

|编号|功能点|工作量|依赖|
|---|---|---|---|
|C02|AUTO\_INCREMENT / SERIAL|中|无|
|M04|INSERT \.\.\. RETURNING|小|C02|
|M06|UPSERT \(ON CONFLICT DO UPDATE\)|中|无|
|T15|JSON 类型|中|无|
|J01|json\_extract|小|T15|
|J02|\-\> / \-\>\> 路径操作符|小|T15|
|C03|NOT NULL 约束|小|无|
|C04|UNIQUE 约束|中|I04|
|C05|FOREIGN KEY \+ CASCADE|大|无|
|C07|DEFAULT 值|小|无|
|D06|ALTER TABLE ADD COLUMN|中|无|
|I03|B\+Tree 二级索引|大|I02|
|I04|唯一索引|小|I03|
|I05|复合索引|中|I03|
|Perf02|Prepared Statement|中|无|
|Q04|IN \(列表\)|小|无|
|Q05|BETWEEN|小|无|
|Q09|IS NULL / IS NOT NULL|小|无|
|Q11|DISTINCT|中|无|
|Q13|ORDER BY 多列|小|无|
|Q17|GROUP BY 多列|小|无|
|A02|COUNT\(col\) / COUNT\(DISTINCT\)|小|无|
|F01|CAST|中|无|
|F02|COALESCE / IFNULL|小|无|
|S01\-S04|核心字符串函数 ×4|中|无|
|S06|REPLACE|小|无|
|N01|ABS|小|无|
|N02|ROUND|小|无|
|N06|MOD|小|无|
|T10|BLOB 类型|中|无|
|T03/T04|TINYINT/SMALLINT 别名|小|无|
|P01|information\_schema|中|无|
|P02|PRAGMA table\_info|小|P01|
|P08|PRAGMA foreign\_keys|小|C05|
|Perf05|Top\-N 排序优化|中|无|

**验收标准**：

- llmRx Store 88 方法中 60\+ 可正确执行

- TPC\-H Q1（简单聚合）可正确执行

- 基础 CRUD \+ 聚合 \+ 过滤 \+ 排序 \+ 分页全通

---

### v0\.15 — Agent 核心差异化（3\-4 周）

**主题**：向量混合查询 \+ TTL \+ 缓存 \+ 限流，建立 Agent 场景护城河

**核心目标**：

- 混合查询（向量\+标量过滤）性能达标

- TTL \+ KV 缓存 \+ 限流三大 Agent 原语落地

- 日志分析场景（时间桶 \+ Top\-N）性能超 SQLite 10×

|编号|功能点|工作量|依赖|
|---|---|---|---|
|V07|混合查询（向量 \+ 标量 pre\-filter）|大|无|
|V08|INT8 向量量化|中|无|
|Ag01|内置 TTL（表级）|中|无|
|Ag02|内置 KV 缓存引擎|大|无|
|Ag03|滑动窗口限流原语|中|无|
|Ag07|对话历史时序优化|中|Dt04|
|T13|DATETIME / TIMESTAMP 类型|中|无|
|Dt01\-Dt03|基础日期函数 ×3|中|T13|
|Dt06|日期加减|中|T13|
|Q27|UNION / UNION ALL|中|无|
|Q23|子查询 \(IN / EXISTS\)|大|无|
|Q29|CASE WHEN|中|无|
|M03|INSERT \.\.\. SELECT|中|无|
|M05|INSERT OR IGNORE / REPLACE|中|M06|
|D02|CREATE TABLE AS SELECT|小|M03|
|D16|TRUNCATE TABLE|小|无|
|J03/J04|json\_object / json\_array|小|T15|
|J06|json\_set / json\_insert / json\_replace|中|T15|
|J08|json\_array\_length|小|T15|
|Q06|LIKE 模糊匹配|中|无|
|Q10|IS / IS NOT|小|无|
|Q18|HAVING|小|Q16|
|Txn05|SAVEPOINT|中|Txn01|
|Txn08|WAL checkpoint 控制|小|Txn02|
|Txn09|只读事务优化|小|Txn03|
|P11|EXPLAIN / EXPLAIN ANALYZE|中|无|
|P03|PRAGMA index\_info / index\_list|小|P01|
|P04\-P06|PRAGMA journal/synchronous/cache\_size|小|无|
|S05|TRIM / LTRIM / RTRIM|小|S01|
|S07|INSTR / POSITION|小|S01|
|S11|SPLIT\_PART|小|S01|
|N03\-N05|CEIL/FLOOR/TRUNC|小|N01|
|N07\-N08|POWER/SQRT|小|N01|
|F03|NULLIF|小|F02|
|F04|IF|小|Q29|
|Eco04|命令行 CLI|中|无|
|Perf07|向量化表达式计算|中|无|

**验收标准**：

- 混合查询：pre\-filter 减少向量搜索范围 50%\+

- TTL：过期数据在 compaction 时自动清理

- 限流原语：100 万次/秒以上的原子递增

- 时间桶聚合查询：比 SQLite 快 10×\+

---

### v0\.16 — 分析能力增强（3\-4 周）

**主题**：JOIN \+ 窗口函数 \+ CTE \+ CBO，分析能力对齐 DuckDB 60%

**核心目标**：

- 完整支持 JOIN 查询

- 窗口函数可用

- CBO 优化器上线

- 分析性能达 DuckDB 60%\+

|编号|功能点|工作量|依赖|
|---|---|---|---|
|Q19|INNER JOIN|大|无|
|Q20|LEFT / RIGHT OUTER JOIN|中|Q19|
|Q21|CROSS JOIN|小|Q19|
|Perf06|向量化 Hash Join|大|Q19|
|Q25|CTE \(WITH\)|中|无|
|Q28|INTERSECT / EXCEPT|中|Q27|
|Q24|标量子查询|中|Q23|
|A07\-A10|聚合扩展 ×4|中|无|
|A11\-A13|窗口函数 ×3 类|大|无|
|Perf10|向量化窗口函数|大|A11|
|Dt04|DATE\_TRUNC / date\_bin|中|Dt01|
|Dt07|日期差|小|Dt01|
|Dt08|STRPTIME|小|Dt01|
|J05|json\_each / json\_tree|中|T15|
|J07|json\_remove|小|J06|
|J09|json\_type / json\_valid|小|T15|
|J10/J11|json\_agg / json\_group\_object|中|T15|
|J12|jsonb 二进制存储|中|T15|
|T16|JSONB 类型|小|J12|
|T07|FLOAT32|小|无|
|T21|ARRAY 类型|中|无|
|D07|ALTER TABLE DROP COLUMN|中|D06|
|D08/D09|ALTER TABLE RENAME|小|D06|
|I12|REINDEX|小|I03|
|V11|多向量字段|中|V01|
|Txn06|事务隔离级别设置|小|Txn03|
|Txn07|死锁检测|中|Txn03|
|D18|ANALYZE|中|无|
|Perf08|CBO 基于代价的优化器|大|D18|
|P07|PRAGMA page\_size / page\_count|小|无|
|P12|统计信息表|中|D18|
|N09|对数函数|小|N01|
|N12|RANDOM|小|无|
|F05|TYPEOF|小|无|
|Q08|REGEXP|中|无|
|Eco03|Python 绑定|大|Eco01|
|Eco05|导出导入 \(CSV/JSON\)|中|无|
|Eco07|ATTACH DATABASE|中|无|
|Eco08|备份与恢复 API|中|Txn02|
|Ag04|分层记忆接口|中|Ag01|
|Ag08|工具调用日志专用表|小|无|

**验收标准**：

- TPC\-H Q1/Q3/Q6/Q12 可正确执行

- JOIN 查询性能：≥ SQLite 5×

- 窗口函数：3 类（排名/偏移/聚合）全覆盖

- CBO：简单查询自动选索引

---

### v0\.17\+ — 生态完善（持续迭代）

**主题**：生产级可用性 \+ 生态扩展

|类别|功能点|
|---|---|
|SQL 高级|递归 CTE、FULL OUTER JOIN、MERGE、窗口函数扩展、Q30 SELECT EXCEPT|
|数据类型|DATE/TIME/DECIMAL/UUID/ENUM|
|索引|全文索引 FTS、表达式索引、部分索引、IVF/PQ 向量量化|
|函数|剩余字符串/数值/日期函数、JSON 完整函数集|
|事务|两阶段提交|
|系统|integrity\_check、PRAGMA optimize|
|Agent|记忆重要性评分 \+ 衰减、全文\+向量混合检索、多模态 Blob、任务状态机|
|性能|多线程并行查询、向量化窗口函数优化|
|生态|C API、SQLite 兼容模式、Parquet 直接查询|

---

## 四、版本依赖关系图

```
v0.13 (性能攻坚)
  │
  ├─ B+Tree 主键索引 ──────────────┐
  ├─ WAL 组提交                    │
  ├─ 行数元数据缓存                │
  └─ 批量 INSERT 优化              │
                                  │
v0.14 (SQL 基础补齐) ◄────────────┘
  │
  ├─ 自增主键 + RETURNING ────────┐
  ├─ UPSERT                       │
  ├─ JSON 基础                    │
  ├─ Prepared Statement           │
  ├─ 外键 + 约束                   │
  ├─ B+Tree 二级索引              │
  └─ 基础函数集 (20+)             │
                                  │
v0.15 (Agent 差异化) ◄────────────┘
  │
  ├─ 混合查询 (向量+标量) ────────┐
  ├─ 内置 TTL                     │
  ├─ KV 缓存引擎                  │
  ├─ 滑动窗口限流                  │
  ├─ 日期时间类型+函数             │
  ├─ 子查询 + UNION               │
  └─ CASE WHEN + 更多函数         │
                                  │
v0.16 (分析增强) ◄────────────────┘
  │
  ├─ JOIN + Hash Join ───────────┐
  ├─ 窗口函数                     │
  ├─ CTE                          │
  ├─ CBO + ANALYZE                │
  ├─ JSON 完整支持                │
  └─ Python 绑定 + ATTACH         │
                                  │
v0.17+ (生态完善) ◄───────────────┘
  └─ 全文索引 / FTS / 多线程 / SQLite 兼容 / ...
```

---

## 五、关键指标

|指标|v0\.12|v0\.13|v0\.14|v0\.15|v0\.16|
|---|---|---|---|---|---|
|**功能点完成数**|41|47|88|138|188|
|**功能完成度**|18%|20%|38%|60%|82%|
|**SQL 兼容性 \(vs SQLite\)**|\~30%|\~35%|\~55%|\~70%|\~85%|
|**OLTP 写入 \(vs SQLite\)**|2%|20%|30%|40%|50%|
|**OLAP 分析 \(vs DuckDB\)**|\~30%|\~30%|\~35%|\~45%|\~60%|
|**Agent 场景就绪度**|低|低|中|高|很高|
|**可替换场景**|无|日志写入|简单 CRUD|Agent 网关|通用嵌入式|

> 注：以上为估算值，需各版本实际测试验证。
> 
> 

