use super::*;

use ExpressionKind::{Predicate as P, Value as V};

/// Expressions as a statement writes them, with the text SQL Server publishes in
/// `definition` for each: a `DEFAULT` (`V`) or a `CHECK` (`P`) over the columns
/// `a int, a2 int, b2 int, b varchar(20), n nvarchar(20), c datetime, d decimal(10,2),
/// [my col] int, [a]]b] int`, some of them qualified by the name of their table.
const NORMALISED: &[(ExpressionKind, &str, &str)] = &[
    (V, "0", "((0))"),
    (V, "(0)", "((0))"),
    (V, "((0))", "((0))"),
    (V, "-1", "((-1))"),
    (V, "(-1)", "((-1))"),
    (V, "- 1", "((-1))"),
    (V, "+1", "((1))"),
    (V, "-(1)", "((-1))"),
    (V, "1.5", "((1.5))"),
    (V, "-1.5", "((-1.5))"),
    (V, ".5", "((0.5))"),
    (V, "1.", "((1.))"),
    (V, "1e3", "((1.0000000000000000e+003))"),
    (V, "1.5E-3", "((1.5000000000000000e-003))"),
    (V, "$5", "(($5.0000))"),
    (V, "$-5.25", "(($-5.2500))"),
    (V, "'x'", "('x')"),
    (V, "''", "('')"),
    (V, "'it''s'", "('it''s')"),
    (V, "N'x'", "(N'x')"),
    (V, "NULL", "(NULL)"),
    (V, "null", "(NULL)"),
    (V, "getdate()", "(getdate())"),
    (V, "GETDATE()", "(getdate())"),
    (V, "GetDate ( )", "(getdate())"),
    (V, "CURRENT_TIMESTAMP", "(getdate())"),
    (V, "sysdatetime()", "(sysdatetime())"),
    (V, "getutcdate()", "(getutcdate())"),
    (V, "newid()", "(newid())"),
    (V, "NEWSEQUENTIALID()", "(newsequentialid())"),
    (V, "0x01", "(0x01)"),
    (V, "0xAbCd", "(0xABCD)"),
    (V, "0x", "(0x)"),
    (V, "1+1", "((1)+(1))"),
    (V, "1 + 2 * 3", "((1)+(2)*(3))"),
    (V, "(1+2)*3", "(((1)+(2))*(3))"),
    (V, "1-(-2)", "((1)-(-2))"),
    (V, "'a'+'b'", "('a'+'b')"),
    (V, "CONVERT(int,1)", "(CONVERT([int],(1)))"),
    (V, "CAST(1 AS int)", "(CONVERT([int],(1)))"),
    (V, "cast('1' as int)", "(CONVERT([int],'1'))"),
    (
        V,
        "CONVERT(varchar(10), getdate(), 103)",
        "(CONVERT([varchar](10),getdate(),(103)))",
    ),
    (V, "user_name()", "(user_name())"),
    (V, "SUSER_SNAME()", "(suser_sname())"),
    (V, "CURRENT_USER", "(user_name())"),
    (V, "SYSTEM_USER", "(suser_sname())"),
    (V, "SESSION_USER", "(user_name())"),
    (V, "USER", "(user_name())"),
    (V, "2147483648", "((2147483648.))"),
    (V, "9223372036854775807", "((9223372036854775807.))"),
    (V, "ABS(-5)", "(abs((-5)))"),
    (V, "UPPER('x')", "(upper('x'))"),
    (V, "DATEPART(year, getdate())", "(datepart(year,getdate()))"),
    (
        V,
        "DATEADD(day, 1, getdate())",
        "(dateadd(day,(1),getdate()))",
    ),
    (
        V,
        "CASE WHEN 1=1 THEN 1 ELSE 0 END",
        "(case when (1)=(1) then (1) else (0) end)",
    ),
    (V, "ISNULL(NULL,1)", "(isnull(NULL,(1)))"),
    (V, "COALESCE(NULL,2)", "(coalesce(NULL,(2)))"),
    (V, "@@SERVERNAME", "(@@servername)"),
    (V, "@@SPID", "(@@spid)"),
    (V, "1", "((1))"),
    (V, "'true'", "('true')"),
    (V, "~1", "(~(1))"),
    (V, "5 % 2", "((5)%(2))"),
    (V, "6/3", "((6)/(3))"),
    (V, "1&3", "((1)&(3))"),
    (V, "1|2", "((1)|(2))"),
    (V, "1^3", "((1)^(3))"),
    (
        V,
        "'x' COLLATE Latin1_General_CS_AS",
        "(('x') collate Latin1_General_CS_AS)",
    ),
    (V, "'2020-01-01'", "('2020-01-01')"),
    (V, "0.0", "((0.0))"),
    (V, "CONCAT('a', 'b', 1)", "(concat('a','b',(1)))"),
    (V, "NEXT VALUE FOR dbo.seq", "(NEXT VALUE FOR [dbo].[seq])"),
    (P, "a>0", "([a]>(0))"),
    (P, "a >= 0", "([a]>=(0))"),
    (P, "a<>0", "([a]<>(0))"),
    (P, "a!=0", "([a]<>(0))"),
    (P, "a !< 0", "([a]>=(0))"),
    (P, "a !> 0", "([a]<=(0))"),
    (P, "a>0 AND b<'z'", "([a]>(0) AND [b]<'z')"),
    (P, "a>0 OR a<-10", "([a]>(0) OR [a]<(-10))"),
    (P, "NOT a>0", "(NOT [a]>(0))"),
    (P, "NOT (a>0)", "(NOT [a]>(0))"),
    (P, "a IS NULL", "([a] IS NULL)"),
    (P, "a IS NOT NULL", "([a] IS NOT NULL)"),
    (P, "a BETWEEN 1 AND 5", "([a]>=(1) AND [a]<=(5))"),
    (P, "a NOT BETWEEN 1 AND 5", "(NOT ([a]>=(1) AND [a]<=(5)))"),
    (P, "a IN (1,2,3)", "([a]=(3) OR [a]=(2) OR [a]=(1))"),
    (P, "a NOT IN (1, 2)", "(NOT ([a]=(2) OR [a]=(1)))"),
    (P, "b LIKE 'x%'", "([b] like 'x%')"),
    (P, "b NOT LIKE 'x%'", "(NOT [b] like 'x%')"),
    (P, "b LIKE 'x!%' ESCAPE '!'", "([b] like 'x!%' escape '!' )"),
    (P, "len(b)>0", "(len([b])>(0))"),
    (P, "LEN(b) > 0", "(len([b])>(0))"),
    (P, "a+1>0", "(([a]+(1))>(0))"),
    (P, "a*2>-1", "([a]*(2)>(-1))"),
    (P, "(a>0)", "([a]>(0))"),
    (P, "((a>0))", "([a]>(0))"),
    (
        P,
        "a > 0 AND (b < 'm' OR a = 5)",
        "([a]>(0) AND ([b]<'m' OR [a]=(5)))",
    ),
    (
        P,
        "(a > 0 AND b < 'm') OR a = 5",
        "([a]>(0) AND [b]<'m' OR [a]=(5))",
    ),
    (
        P,
        "a > 0 AND b < 'm' OR a = 5",
        "([a]>(0) AND [b]<'m' OR [a]=(5))",
    ),
    (
        P,
        "a > 0 OR b < 'm' AND a = 5",
        "([a]>(0) OR [b]<'m' AND [a]=(5))",
    ),
    (
        P,
        "CASE WHEN a>0 THEN 1 ELSE 0 END=1",
        "(case when [a]>(0) then (1) else (0) end=(1))",
    ),
    (
        P,
        "CASE a WHEN 1 THEN 'x' END = 'x'",
        "(case [a] when (1) then 'x'  end='x')",
    ),
    (P, "a > -1", "([a]>(-1))"),
    (P, "b <> ''", "([b]<>'')"),
    (P, "n = N'x'", "([n]=N'x')"),
    (P, "d=1.5", "([d]=(1.5))"),
    (P, "CAST(a AS bigint)>0", "(CONVERT([bigint],[a])>(0))"),
    (
        P,
        "CONVERT(varchar(10),a)<>''",
        "(CONVERT([varchar](10),[a])<>'')",
    ),
    (P, "ISNULL(a,0)>0", "(isnull([a],(0))>(0))"),
    (P, "COALESCE(a,0) > 0", "(coalesce([a],(0))>(0))"),
    (P, "a % 2 = 0", "([a]%(2)=(0))"),
    (P, "a - a2 > 0", "(([a]-[a2])>(0))"),
    (P, "a/2>0", "([a]/(2)>(0))"),
    (P, "-a<0", "(( -[a])<(0))"),
    (P, "~a<>0", "(~[a]<>(0))"),
    (P, "a&1=0", "(([a]&(1))=(0))"),
    (P, "upper(b)='X'", "(upper([b])='X')"),
    (
        P,
        "b COLLATE Latin1_General_CS_AS = 'x'",
        "(([b]) collate Latin1_General_CS_AS='x')",
    ),
    (P, "[a]>0", "([a]>(0))"),
    (P, "[my col] > 0", "([my col]>(0))"),
    (P, "a2 > a", "([a2]>[a])"),
    (P, "c < getdate()", "([c]<getdate())"),
    (P, "c>'2020-01-01'", "([c]>'2020-01-01')"),
    (P, "a = 2147483648", "([a]=(2147483648.))"),
    (P, "a=+1", "([a]=(1))"),
    (P, "a = -(1)", "([a]=(-1))"),
    (P, "a = 1 + 2 * 3", "([a]=((1)+(2)*(3)))"),
    (P, "a = (1 + 2) * 3", "([a]=((1)+(2))*(3))"),
    (P, "a*(a2+1)>0", "([a]*([a2]+(1))>(0))"),
    (P, "a-(a2-1)>0", "(([a]-([a2]-(1)))>(0))"),
    (P, "a-a2-1>0", "((([a]-[a2])-(1))>(0))"),
    (
        P,
        "a > 0 AND a < 10 AND a <> 5",
        "([a]>(0) AND [a]<(10) AND [a]<>(5))",
    ),
    (
        P,
        "a = 1 OR a = 2 OR a = 3",
        "([a]=(1) OR [a]=(2) OR [a]=(3))",
    ),
    (P, "NOT a IS NULL", "(NOT [a] IS NULL)"),
    (
        P,
        "NOT (a IS NULL OR b IS NULL)",
        "(NOT ([a] IS NULL OR [b] IS NULL))",
    ),
    (P, "a IN (1)", "([a]=(1))"),
    (P, "b IN ('a','b')", "([b]='b' OR [b]='a')"),
    (P, "a=0x01", "([a]=0x01)"),
    (P, "b = 'it''s'", "([b]='it''s')"),
    (P, "DATALENGTH(b) < 10", "(datalength([b])<(10))"),
    (P, "b > N'a'", "([b]>N'a')"),
    (P, "c = CURRENT_TIMESTAMP", "([c]=getdate())"),
    (P, "a = @@SPID", "([a]=@@spid)"),
    (V, "007", "((7))"),
    (V, "0.50", "((0.50))"),
    (V, "1.50e0", "((1.5000000000000000e+000))"),
    (V, "-2147483648", "((-2147483648))"),
    (V, "-0", "((0))"),
    (V, "'a' + 'b' + 'c'", "(('a'+'b')+'c')"),
    (V, "1 + 2 + 3", "(((1)+(2))+(3))"),
    (V, "1 - 2 - 3", "(((1)-(2))-(3))"),
    (V, "1 - (2 - 3)", "((1)-((2)-(3)))"),
    (V, "2 * 3 * 4", "(((2)*(3))*(4))"),
    (V, "2 * (3 * 4)", "((2)*((3)*(4)))"),
    (V, "12 / 2 / 3", "(((12)/(2))/(3))"),
    (V, "-(-1)", "((1))"),
    (V, "-(1+1)", "( -((1)+(1)))"),
    (V, "+(1)", "((1))"),
    (V, "- -1", "((1))"),
    (V, "1.50", "((1.50))"),
    (V, "00.5", "((0.5))"),
    (V, "0x0a", "(0x0A)"),
    (V, "N''", "(N'')"),
    (V, "CAST(NULL AS int)", "(CONVERT([int],NULL))"),
    (
        V,
        "CONVERT(datetime, '2020-01-01')",
        "(CONVERT([datetime],'2020-01-01'))",
    ),
    (
        V,
        "CONVERT(nvarchar(max), N'x')",
        "(CONVERT([nvarchar](max),N'x'))",
    ),
    (
        V,
        "CAST(1 AS decimal(10, 2))",
        "(CONVERT([decimal](10,2),(1)))",
    ),
    (V, "CAST(0 AS bit)", "(CONVERT([bit],(0)))"),
    (V, "CAST(1 AS float)", "(CONVERT([float],(1)))"),
    (V, "CAST('x' AS varchar)", "(CONVERT([varchar],'x'))"),
    (V, "SYSUTCDATETIME()", "(sysutcdatetime())"),
    (V, "SYSDATETIMEOFFSET()", "(sysdatetimeoffset())"),
    (V, "HOST_NAME()", "(host_name())"),
    (V, "APP_NAME()", "(app_name())"),
    (V, "DB_NAME()", "(db_name())"),
    (V, "@@VERSION", "(@@version)"),
    (V, "1e0", "((1.0000000000000000e+000))"),
    (V, "2.5E+10", "((2.5000000000000000e+010))"),
    (V, "$0", "(($0.0000))"),
    (V, "$1.23456", "(($1.2346))"),
    (V, "ISNULL(NULL, 'x')", "(isnull(NULL,'x'))"),
    (V, "COALESCE(NULL, NULL, 3)", "(coalesce(NULL,NULL,(3)))"),
    (V, "NULLIF(1, 0)", "(nullif((1),(0)))"),
    (V, "LEFT('abc', 1)", "(left('abc',(1)))"),
    (V, "(1)*(-1)", "((1)*(-1))"),
    (V, "~~1", "(~(~(1)))"),
    (V, "-~1", "( -(~(1)))"),
    (V, "~-1", "(~(-1))"),
    (V, "-1e3", "((-1.0000000000000000e+003))"),
    (V, "-$5", "(($-5.0000))"),
    (V, "PI()", "(pi())"),
    (V, "ROUND(1.5, 0)", "(round((1.5),(0)))"),
    (V, "SPACE(1)", "(space((1)))"),
    (V, "1 + -1", "((1)+(-1))"),
    (V, "1 * -1", "((1)*(-1))"),
    (V, "(1)", "((1))"),
    (V, "REPLICATE('a', 3)", "(replicate('a',(3)))"),
    (
        V,
        "DATEADD(dd, -1, GETDATE())",
        "(dateadd(day,(-1),getdate()))",
    ),
    (
        V,
        "DATEDIFF(day, '2020-01-01', getdate())",
        "(datediff(day,'2020-01-01',getdate()))",
    ),
    (V, "YEAR(getdate())", "(datepart(year,getdate()))"),
    (V, "FORMAT(1, 'N')", "(format((1),'N'))"),
    (V, "TRY_CAST('1' AS int)", "(TRY_CAST('1' AS [int]))"),
    (V, "TRY_CONVERT(int, '1')", "(TRY_CAST('1' AS [int]))"),
    (V, "CHECKSUM(1)", "(checksum((1)))"),
    (V, "1", "((1))"),
    (V, "1.5e-10", "((1.5000000000000000e-010))"),
    (V, "123456789012345678.0e5", "((1.2345678901234568e+022))"),
    (V, "1.5e0", "((1.5000000000000000e+000))"),
    (V, "12345678901234567890123", "((12345678901234567890123.))"),
    (
        V,
        "CASE 1 WHEN 1 THEN 2 WHEN 3 THEN 4 ELSE 5 END",
        "(case (1) when (1) then (2) when (3) then (4) else (5) end)",
    ),
    (
        V,
        "'a' + CAST(1 AS varchar(10))",
        "('a'+CONVERT([varchar](10),(1)))",
    ),
    (V, "1 + (2)", "((1)+(2))"),
    (V, "(1 + 2)", "((1)+(2))"),
    (V, "((1 + 2))", "((1)+(2))"),
    (P, "a*a2*b2>0", "(([a]*[a2])*[b2]>(0))"),
    (P, "a*(a2*b2)>0", "([a]*([a2]*[b2])>(0))"),
    (P, "(a*2)>0", "([a]*(2)>(0))"),
    (P, "a+(1+2)>0", "(([a]+((1)+(2)))>(0))"),
    (P, "a+1+2>0", "((([a]+(1))+(2))>(0))"),
    (
        P,
        "a=1 OR (a=2 OR a=3)",
        "([a]=(1) OR ([a]=(2) OR [a]=(3)))",
    ),
    (P, "(a=1 OR a=2) OR a=3", "([a]=(1) OR [a]=(2) OR [a]=(3))"),
    (
        P,
        "a=1 AND (a2=2 AND b='x')",
        "([a]=(1) AND ([a2]=(2) AND [b]='x'))",
    ),
    (
        P,
        "(a=1 AND a2=2) AND b='x'",
        "([a]=(1) AND [a2]=(2) AND [b]='x')",
    ),
    (P, "-(a+1)<0", "(( -([a]+(1)))<(0))"),
    (P, "- -a<0", "(( -( -[a]))<(0))"),
    (P, "NOT NOT a>0", "(NOT NOT [a]>(0))"),
    (P, "~(a+1)<>0", "(~([a]+(1))<>(0))"),
    (P, "a IN (a2, 1+1)", "([a]=((1)+(1)) OR [a]=[a2])"),
    (P, "[a]]b] > 0", "([a]]b]>(0))"),
    (P, "b LIKE N'x%'", "([b] like N'x%')"),
    (P, "b LIKE 'x' + '%'", "([b] like 'x'+'%')"),
    (P, "CONVERT(varchar, a)<>''", "(CONVERT([varchar],[a])<>'')"),
    (
        P,
        "CONVERT(varchar(max), a)<>''",
        "(CONVERT([varchar](max),[a])<>'')",
    ),
    (
        P,
        "CAST(a AS decimal(10,2))>0",
        "(CONVERT([decimal](10,2),[a])>(0))",
    ),
    (
        P,
        "CAST(a AS nvarchar(max))<>N''",
        "(CONVERT([nvarchar](max),[a])<>N'')",
    ),
    (P, "TRY_CAST(a AS int) > 0", "(TRY_CAST([a] AS [int])>(0))"),
    (P, "COALESCE(a, a2, 0)>0", "(coalesce([a],[a2],(0))>(0))"),
    (P, "NULLIF(a,0) > 0", "(nullif([a],(0))>(0))"),
    (P, "a = NULL", "([a]=NULL)"),
    (P, "b + 'x' = 'yx'", "(([b]+'x')='yx')"),
    (
        P,
        "a IS NULL AND b IS NULL",
        "([a] IS NULL AND [b] IS NULL)",
    ),
    (P, "LEFT(b,1)='x'", "(left([b],(1))='x')"),
    (P, "DATEADD(dd, 1, c) > c", "(dateadd(day,(1),[c])>[c])"),
    (
        P,
        "DATEDIFF(day, c, getdate()) < 5",
        "(datediff(day,[c],getdate())<(5))",
    ),
    (P, "YEAR(c) > 2000", "(datepart(year,[c])>(2000))"),
    (P, "CHARINDEX('x', b) > 0", "(charindex('x',[b])>(0))"),
    (P, "SUBSTRING(b,1,2)='ab'", "(substring([b],(1),(2))='ab')"),
    (P, "LTRIM(RTRIM(b)) <> ''", "(ltrim(rtrim([b]))<>'')"),
    (P, "ROUND(d, 1) > 0", "(round([d],(1))>(0))"),
    (
        P,
        "CASE WHEN a>0 THEN 'x' WHEN a<0 THEN 'y' END='x'",
        "(case when [a]>(0) then 'x' when [a]<(0) then 'y'  end='x')",
    ),
    (
        P,
        "CASE WHEN a > 0 THEN 1 END = 1",
        "(case when [a]>(0) then (1)  end=(1))",
    ),
    (P, "a > 0 AND NOT b = 'x'", "([a]>(0) AND NOT [b]='x')"),
    (P, "NOT (a > 0) AND b = 'x'", "(NOT [a]>(0) AND [b]='x')"),
    (
        P,
        "NOT (a > 0 AND b = 'x') OR a2 = 1",
        "(NOT ([a]>(0) AND [b]='x') OR [a2]=(1))",
    ),
    (
        P,
        "a2 BETWEEN a AND a + 10",
        "([a2]>=[a] AND [a2]<=([a]+(10)))",
    ),
    (
        P,
        "b NOT LIKE 'x%' ESCAPE '!'",
        "(NOT [b] like 'x%' escape '!' )",
    ),
    (
        P,
        "a = 1 AND a2 = 2 OR b = 'x' AND n = N'y'",
        "([a]=(1) AND [a2]=(2) OR [b]='x' AND [n]=N'y')",
    ),
    (
        P,
        "(a = 1 OR a2 = 2) AND (b = 'x' OR n = N'y')",
        "(([a]=(1) OR [a2]=(2)) AND ([b]='x' OR [n]=N'y'))",
    ),
    (P, "a+a2*3>0", "(([a]+[a2]*(3))>(0))"),
    (P, "(a+a2)*3>0", "(([a]+[a2])*(3)>(0))"),
    (P, "a*3+a2>0", "(([a]*(3)+[a2])>(0))"),
    (P, "a-a2+1>0", "((([a]-[a2])+(1))>(0))"),
    (P, "a/(a2*2)>0", "([a]/([a2]*(2))>(0))"),
    (P, "a%a2%2=0", "(([a]%[a2])%(2)=(0))"),
    (P, "a&a2|1>0", "((([a]&[a2])|(1))>(0))"),
    (P, "a|a2&1>0", "((([a]|[a2])&(1))>(0))"),
    (P, "a^1>0", "(([a]^(1))>(0))"),
    (P, "a=a2", "([a]=[a2])"),
    (P, "1=1", "((1)=(1))"),
    (P, "a2 < a OR a2 IS NULL", "([a2]<[a] OR [a2] IS NULL)"),
    (P, "b IN ('x')", "([b]='x')"),
    (P, "b NOT IN ('x', 'y')", "(NOT ([b]='y' OR [b]='x'))"),
    (P, "a NOT IN (1)", "(NOT [a]=(1))"),
    (P, "a IN (1, NULL)", "([a]=NULL OR [a]=(1))"),
    (
        P,
        "c BETWEEN '2020-01-01' AND '2021-01-01'",
        "([c]>='2020-01-01' AND [c]<='2021-01-01')",
    ),
    (P, "d > -0.5", "([d]>(-0.5))"),
    (P, "d >= 0.0", "([d]>=(0.0))"),
    (P, "a > 1.5e0", "([a]>(1.5000000000000000e+000))"),
    (P, "d < $5", "([d]<($5.0000))"),
    (P, "NOT a IN (1,2)", "(NOT ([a]=(2) OR [a]=(1)))"),
    (P, "NOT a BETWEEN 1 AND 2", "(NOT ([a]>=(1) AND [a]<=(2)))"),
    (P, "NOT b LIKE 'x'", "(NOT [b] like 'x')"),
    (P, "(b) LIKE 'x'", "([b] like 'x')"),
    (P, "b LIKE ('x')", "([b] like 'x')"),
    (P, "(a) IS NULL", "([a] IS NULL)"),
    (P, "a + 1 IS NULL", "(([a]+(1)) IS NULL)"),
    (P, "ISNULL(b,'') <> ''", "(isnull([b],'')<>'')"),
    (P, "(((a)))>(((0)))", "([a]>(0))"),
    (P, "a <= 0x0A", "([a]<=0x0A)"),
    (
        P,
        "len(b) between 1 and 5",
        "(len([b])>=(1) AND len([b])<=(5))",
    ),
    (
        P,
        "b > 'a' COLLATE Latin1_General_CS_AS",
        "([b]>('a') collate Latin1_General_CS_AS)",
    ),
    (P, "abs(-a)>0", "(abs( -[a])>(0))"),
    (
        P,
        "CONVERT(varchar(10),-a)<>''",
        "(CONVERT([varchar](10), -[a])<>'')",
    ),
    (P, "CAST(-a AS bigint)>0", "(CONVERT([bigint], -[a])>(0))"),
    (
        P,
        "CASE WHEN a>0 THEN -a ELSE -a2 END>0",
        "(case when [a]>(0) then  -[a] else  -[a2] end>(0))",
    ),
    (
        P,
        "CASE -a WHEN 1 THEN 1 END=1",
        "(case  -[a] when (1) then (1)  end=(1))",
    ),
    (
        P,
        "CASE WHEN a>0 THEN -(a+1) END>0",
        "(case when [a]>(0) then  -([a]+(1))  end>(0))",
    ),
    (P, "coalesce(-a,0)>0", "(coalesce( -[a],(0))>(0))"),
    (P, "-a IN (1,2)", "(( -[a])=(2) OR ( -[a])=(1))"),
    (P, "-a IS NULL", "(( -[a]) IS NULL)"),
    (P, "-a BETWEEN 1 AND 2", "(( -[a])>=(1) AND ( -[a])<=(2))"),
    (
        P,
        "CASE WHEN -a>0 THEN 1 END=1",
        "(case when ( -[a])>(0) then (1)  end=(1))",
    ),
    (P, "abs(-(-a))>0", "(abs( -( -[a]))>(0))"),
    (P, "abs(-~a)>0", "(abs( -(~[a]))>(0))"),
    (P, "isnull(-a, -1)>0", "(isnull( -[a],(-1))>(0))"),
    (P, "abs(~a)>0", "(abs(~[a])>(0))"),
    (
        P,
        "CASE WHEN a>0 THEN ~a END>0",
        "(case when [a]>(0) then ~[a]  end>(0))",
    ),
    (P, "q1.a > 0", "([q1].[a]>(0))"),
    (P, "[q3].[my col] > 0", "([q3].[my col]>(0))"),
    (
        P,
        "q4.a + q4.a2 > 0 AND abs(q4.a) IN (1, 2)",
        "(([q4].[a]+[q4].[a2])>(0) AND (abs([q4].[a])=(2) OR abs([q4].[a])=(1)))",
    ),
    (P, "[q]]5].a > 0", "([q]]5].[a]>(0))"),
    (P, "Q6.A > 0", "([Q6].[A]>(0))"),
];

/// The tree of `text` as a statement writes it.
fn written(kind: ExpressionKind, text: &str) -> Expr {
    parse_stored_expression(text, kind)
        .unwrap_or_else(|error| panic!("{text} does not parse: {error:?}"))
}

#[test]
fn the_definition_text_is_normalised() {
    let mut wrong = Vec::new();
    for (kind, input, expected) in NORMALISED {
        let text = normalized_text(&written(*kind, input));
        if text.as_deref() != Some(*expected) {
            wrong.push(format!("{input}: {text:?}, expected {expected}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        NORMALISED.len(),
        wrong.join("\n")
    );
    assert_eq!(NORMALISED.len(), 313);
}

/// The text published for `expr`: the normalised text, or `Display` between parentheses.
fn definition_text(expr: &Expr) -> String {
    normalized_text(expr).unwrap_or_else(|| display_text(expr))
}

#[test]
fn the_text_of_a_stored_tree_is_the_stored_text() {
    for (kind, input, expected) in NORMALISED {
        let text = stored_text(&written(*kind, input), *kind).expect(input);
        assert_eq!(text, *expected, "{input}");
        let tree = parse_stored_expression(&text, *kind).expect(input);
        assert_eq!(definition_text(&tree), text, "{input}");
    }
}

#[test]
fn a_tree_rebuilt_from_its_text_is_the_stored_tree() {
    for (kind, input, _) in NORMALISED {
        let text = stored_text(&written(*kind, input), *kind).expect(input);
        let rebuilt = parse_stored_expression(&text, *kind).expect(input);
        let again = stored_text(&rebuilt, *kind).expect(input);
        assert_eq!(again, text, "{input}");
        assert_eq!(
            parse_stored_expression(&again, *kind).expect(input),
            rebuilt,
            "{input}"
        );
    }
}

#[test]
fn a_shape_left_unnormalised_keeps_the_text_display_writes() {
    // A column qualified by a schema and a subquery are not normalised: the text is
    // `Display` between one pair of parentheses, and reads back to the same text.
    for (kind, input) in [
        (P, "dbo.t.a > 0"),
        (P, "(dbo.t.a > 0)"),
        (V, "(SELECT 1)"),
        (P, "a IN (SELECT 1)"),
    ] {
        let tree = written(kind, input);
        assert_eq!(normalized_text(&tree), None, "{input}");
        let text = stored_text(&tree, kind).expect(input);
        assert_eq!(text, display_text(&tree), "{input}");
        let stored = parse_stored_expression(&text, kind).expect(input);
        assert_eq!(definition_text(&stored), text, "{input}");
        assert_eq!(stored_text(&stored, kind).expect(input), text);
    }
}

#[test]
fn a_bracketed_type_name_reads_back_bare() {
    let tree = parse_stored_expression("(CONVERT([varchar](10),[a])<>'')", P).expect("parse");
    let Expr::Nested(inner, _) = &tree else {
        panic!("{tree:?}");
    };
    let Expr::Binary { left, .. } = inner.as_ref() else {
        panic!("{inner:?}");
    };
    let Expr::Convert { ty, .. } = left.as_ref() else {
        panic!("{left:?}");
    };
    assert_eq!(ty.name, "varchar");
    assert_eq!(ty.args, vec![TypeArg::Number(10)]);
}
