CREATE TABLE supplier (s_suppkey INT, s_name CHARACTER VARYING, s_address CHARACTER VARYING, s_nationkey INT, s_phone CHARACTER VARYING, s_acctbal NUMERIC, s_comment CHARACTER VARYING, PRIMARY KEY (s_suppkey));
CREATE TABLE part (p_partkey INT, p_name CHARACTER VARYING, p_mfgr CHARACTER VARYING, p_brand CHARACTER VARYING, p_type CHARACTER VARYING, p_size INT, p_container CHARACTER VARYING, p_retailprice NUMERIC, p_comment CHARACTER VARYING, PRIMARY KEY (p_partkey));
CREATE TABLE partsupp (ps_partkey INT, ps_suppkey INT, ps_availqty INT, ps_supplycost NUMERIC, ps_comment CHARACTER VARYING, PRIMARY KEY (ps_partkey, ps_suppkey));
CREATE TABLE customer (c_custkey INT, c_name CHARACTER VARYING, c_address CHARACTER VARYING, c_nationkey INT, c_phone CHARACTER VARYING, c_acctbal NUMERIC, c_mktsegment CHARACTER VARYING, c_comment CHARACTER VARYING, PRIMARY KEY (c_custkey));
CREATE TABLE orders (o_orderkey BIGINT, o_custkey INT, o_orderstatus CHARACTER VARYING, o_totalprice NUMERIC, o_orderdate DATE, o_orderpriority CHARACTER VARYING, o_clerk CHARACTER VARYING, o_shippriority INT, o_comment CHARACTER VARYING, PRIMARY KEY (o_orderkey));
CREATE TABLE lineitem (l_orderkey BIGINT, l_partkey INT, l_suppkey INT, l_linenumber INT, l_quantity NUMERIC, l_extendedprice NUMERIC, l_discount NUMERIC, l_tax NUMERIC, l_returnflag CHARACTER VARYING, l_linestatus CHARACTER VARYING, l_shipdate DATE, l_commitdate DATE, l_receiptdate DATE, l_shipinstruct CHARACTER VARYING, l_shipmode CHARACTER VARYING, l_comment CHARACTER VARYING, PRIMARY KEY (l_orderkey, l_linenumber));
CREATE TABLE nation (n_nationkey INT, n_name CHARACTER VARYING, n_regionkey INT, n_comment CHARACTER VARYING, PRIMARY KEY (n_nationkey));
CREATE TABLE region (r_regionkey INT, r_name CHARACTER VARYING, r_comment CHARACTER VARYING, PRIMARY KEY (r_regionkey));
CREATE TABLE person (id BIGINT, name CHARACTER VARYING, email_address CHARACTER VARYING, credit_card CHARACTER VARYING, city CHARACTER VARYING, state CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING, PRIMARY KEY (id));
CREATE TABLE auction (id BIGINT, item_name CHARACTER VARYING, description CHARACTER VARYING, initial_bid BIGINT, reserve BIGINT, date_time TIMESTAMP, expires TIMESTAMP, seller BIGINT, category BIGINT, extra CHARACTER VARYING, PRIMARY KEY (id));
CREATE TABLE bid (auction BIGINT, bidder BIGINT, price BIGINT, channel CHARACTER VARYING, url CHARACTER VARYING, date_time TIMESTAMP, extra CHARACTER VARYING);
CREATE TABLE alltypes1 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);
CREATE TABLE alltypes2 (c1 BOOLEAN, c2 SMALLINT, c3 INT, c4 BIGINT, c5 REAL, c6 DOUBLE, c7 NUMERIC, c8 DATE, c9 CHARACTER VARYING, c10 TIME, c11 TIMESTAMP, c13 INTERVAL, c14 STRUCT<a INT>, c15 INT[], c16 CHARACTER VARYING[]);
CREATE MATERIALIZED VIEW m0 AS SELECT DATE '2022-03-03' AS col_0, 'TCbxc7iF06' AS col_1, (REAL '2147483647') AS col_2, t_0.o_comment AS col_3 FROM orders AS t_0 GROUP BY t_0.o_comment, t_0.o_orderdate, t_0.o_totalprice, t_0.o_orderkey HAVING ((FLOAT '1') < (FLOAT '872'));
CREATE MATERIALIZED VIEW m1 AS WITH with_0 AS (SELECT (- t_1.ps_supplycost) AS col_0, t_2.n_name AS col_1 FROM partsupp AS t_1 JOIN nation AS t_2 ON t_1.ps_comment = t_2.n_comment AND true WHERE false GROUP BY t_1.ps_supplycost, t_2.n_name) SELECT (INT '2147483647') AS col_0 FROM with_0 WHERE true;
CREATE MATERIALIZED VIEW m2 AS SELECT (INT '0') AS col_0, t_0.s_comment AS col_1, t_0.s_nationkey AS col_2 FROM supplier AS t_0 WHERE false GROUP BY t_0.s_nationkey, t_0.s_comment HAVING true;
CREATE MATERIALIZED VIEW m3 AS SELECT (coalesce(NULL, NULL, NULL, NULL, NULL, (INT '76'), NULL, NULL, NULL, NULL)) AS col_0, sq_2.col_3 AS col_1 FROM (SELECT t_0.o_comment AS col_0, t_0.o_comment AS col_1, t_0.o_orderpriority AS col_2, t_0.o_comment AS col_3 FROM orders AS t_0 LEFT JOIN m0 AS t_1 ON t_0.o_orderdate = t_1.col_0 AND true WHERE true GROUP BY t_0.o_comment, t_0.o_orderpriority, t_0.o_clerk HAVING ((42) = (BIGINT '983'))) AS sq_2 GROUP BY sq_2.col_0, sq_2.col_3 HAVING (false);
CREATE MATERIALIZED VIEW m4 AS SELECT hop_0.date_time AS col_0 FROM hop(bid, bid.date_time, INTERVAL '60', INTERVAL '540') AS hop_0 GROUP BY hop_0.price, hop_0.date_time, hop_0.url, hop_0.extra HAVING true;
CREATE MATERIALIZED VIEW m5 AS SELECT t_0.n_comment AS col_0 FROM nation AS t_0 FULL JOIN m3 AS t_1 ON t_0.n_name = t_1.col_1 GROUP BY t_0.n_comment, t_0.n_name, t_1.col_1 HAVING false;
CREATE MATERIALIZED VIEW m6 AS SELECT min(t_2.c3) FILTER(WHERE (false)) AS col_0, t_2.c6 AS col_1 FROM alltypes2 AS t_2 WHERE t_2.c1 GROUP BY t_2.c14, t_2.c8, t_2.c6, t_2.c16, t_2.c11, t_2.c3 HAVING false;
CREATE MATERIALIZED VIEW m7 AS SELECT t_2.p_mfgr AS col_0, t_2.p_type AS col_1 FROM part AS t_2 GROUP BY t_2.p_type, t_2.p_partkey, t_2.p_mfgr;
CREATE MATERIALIZED VIEW m8 AS WITH with_0 AS (WITH with_1 AS (SELECT TIMESTAMP '2022-03-02 13:05:36' AS col_0 FROM (SELECT t_2.o_totalprice AS col_0 FROM orders AS t_2 WHERE false GROUP BY t_2.o_totalprice) AS sq_3 WHERE (true IS NULL) GROUP BY sq_3.col_0) SELECT (BIGINT '4') AS col_0, DATE '2022-03-03' AS col_1, (INTERVAL '517940') AS col_2 FROM with_1 WHERE false) SELECT (SMALLINT '17697') AS col_0, (TIME '13:05:36' + (INTERVAL '-1')) AS col_1 FROM with_0 WHERE true;
CREATE MATERIALIZED VIEW m9 AS SELECT sq_2.col_0 AS col_0, CAST(false AS INT) AS col_1 FROM (SELECT t_0.col_0 AS col_0, t_0.col_0 AS col_1, (INT '793') AS col_2 FROM m3 AS t_0 RIGHT JOIN region AS t_1 ON t_0.col_1 = t_1.r_name WHERE true GROUP BY t_0.col_0 HAVING false) AS sq_2 WHERE ((BIGINT '9223372036854775807') < (REAL '104')) GROUP BY sq_2.col_0 HAVING true;
