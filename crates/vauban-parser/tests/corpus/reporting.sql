-- Batches in the style of hand-written reporting queries: aggregates over several
-- GROUP BY columns, HAVING, UNION ALL, derived tables, nested CASE, CONVERT with styles,
-- ROW_NUMBER() OVER (…), TOP … PERCENT WITH TIES, EXCEPT / INTERSECT.
-- Written from general knowledge of such queries; no table, column or
-- schema of an identifiable project.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch report_revenue_by_month_and_region
SELECT YEAR(o.OrderDate) AS [Year], MONTH(o.OrderDate) AS [Month], c.Region,
       COUNT(DISTINCT o.Id) AS Orders,
       SUM(l.Quantity * l.UnitPrice) AS Revenue,
       AVG(l.Quantity * l.UnitPrice) AS AvgLine
FROM dbo.Orders o
INNER JOIN dbo.OrderLines l ON l.OrderId = o.Id
INNER JOIN dbo.Customers c ON c.Id = o.CustomerId
WHERE o.OrderDate >= DATEADD(year, -1, GETDATE())
  AND o.Status <> N'Cancelled'
GROUP BY YEAR(o.OrderDate), MONTH(o.OrderDate), c.Region
HAVING SUM(l.Quantity * l.UnitPrice) > 1000
ORDER BY [Year] DESC, [Month] DESC, Revenue DESC

-- @batch report_union_all_kpis
SELECT N'Orders' AS Metric, COUNT(*) AS Value FROM dbo.Orders
UNION ALL
SELECT N'Customers', COUNT(*) FROM dbo.Customers
UNION ALL
SELECT N'Revenue', CONVERT(int, SUM(Total)) FROM dbo.Orders WHERE Status = N'Paid'
UNION ALL
SELECT N'Open', COUNT(*) FROM dbo.Orders WHERE Status IN (N'New', N'Pending')

-- @batch report_derived_table_and_nested_case
SELECT t.CustomerId, t.OrderCount, t.Revenue,
    CASE
        WHEN t.OrderCount >= 10 THEN CASE WHEN t.Revenue > 10000 THEN N'Gold' ELSE N'Silver' END
        WHEN t.OrderCount >= 3 THEN N'Bronze'
        ELSE N'New'
    END AS Tier,
    CASE t.Region WHEN N'EU' THEN 1 WHEN N'NA' THEN 2 ELSE 9 END AS RegionRank
FROM (
    SELECT o.CustomerId, c.Region, COUNT(*) AS OrderCount, SUM(o.Total) AS Revenue
    FROM dbo.Orders o
    INNER JOIN dbo.Customers c ON c.Id = o.CustomerId
    GROUP BY o.CustomerId, c.Region
) AS t
ORDER BY t.Revenue DESC, t.CustomerId

-- @batch report_convert_styles_and_try_convert
SELECT CONVERT(varchar(10), o.OrderDate, 120) AS OrderDay,
       CONVERT(varchar(23), o.CreatedAt, 121) AS CreatedAt,
       CONVERT(nvarchar(30), o.Total, 1) AS TotalText,
       CAST(o.Total AS decimal(18, 2)) AS Total,
       TRY_CONVERT(int, o.ExternalRef) AS ExternalRef,
       TRY_CAST(o.Notes AS xml) AS NotesXml,
       CONVERT(varchar(8), o.OrderDate, 112) + '-' + RIGHT('000000' + CAST(o.Id AS varchar(6)), 6) AS Reference
FROM dbo.Orders o
WHERE CONVERT(date, o.OrderDate) = CONVERT(date, @Day)

-- @batch report_row_number_first_per_group
SELECT Id, Name, Total
FROM (
    SELECT o.Id, c.Name, o.Total,
           ROW_NUMBER() OVER (PARTITION BY o.CustomerId ORDER BY o.Total DESC, o.Id) AS rn
    FROM dbo.Orders o
    INNER JOIN dbo.Customers c ON c.Id = o.CustomerId
) AS ranked
WHERE rn = 1
ORDER BY Total DESC

-- @batch report_top_percent_with_ties
SELECT TOP (10) PERCENT WITH TIES p.Name, SUM(l.Quantity) AS Sold
FROM dbo.OrderLines l
INNER JOIN dbo.Products p ON p.Id = l.ProductId
GROUP BY p.Name
ORDER BY Sold DESC

-- @batch report_except_and_intersect
SELECT CustomerId FROM dbo.Orders WHERE OrderDate >= '2024-01-01'
EXCEPT
SELECT CustomerId FROM dbo.Orders WHERE OrderDate < '2024-01-01';
SELECT CustomerId FROM dbo.Orders WHERE Status = N'Paid'
INTERSECT
SELECT CustomerId FROM dbo.Invoices WHERE Paid = 1

-- @batch report_scalar_subqueries_in_select_list
SELECT c.Id, c.Name,
    (SELECT COUNT(*) FROM dbo.Orders o WHERE o.CustomerId = c.Id) AS OrderCount,
    (SELECT MAX(o.OrderDate) FROM dbo.Orders o WHERE o.CustomerId = c.Id) AS LastOrder,
    ISNULL((SELECT SUM(o.Total) FROM dbo.Orders o WHERE o.CustomerId = c.Id), 0) AS Revenue
FROM dbo.Customers c
WHERE c.Region IN (N'EU', N'NA')
  AND c.Name NOT LIKE N'Test\_%' ESCAPE N'\'
  AND c.Id NOT IN (SELECT CustomerId FROM dbo.Blacklist)
ORDER BY Revenue DESC, c.Name

-- @batch report_dateparts_and_percentages
SELECT DATEPART(weekday, o.OrderDate) AS Weekday,
       DATENAME(month, o.OrderDate) AS MonthName,
       AVG(o.Total) AS AvgTotal, MIN(o.Total) AS MinTotal, MAX(o.Total) AS MaxTotal,
       SUM(CASE WHEN o.Total > 100 THEN 1 ELSE 0 END) * 100.0 / COUNT(*) AS PctLarge,
       COUNT(*) AS N
FROM dbo.Orders o WITH (NOLOCK)
WHERE o.OrderDate BETWEEN @From AND @To
GROUP BY DATEPART(weekday, o.OrderDate), DATENAME(month, o.OrderDate)
ORDER BY 1, 2

-- @batch report_cross_join_calendar_with_outer_joins
SELECT r.Region, m.MonthNumber,
       ISNULL(s.Revenue, 0) AS Revenue,
       COALESCE(t.Target, 0) AS Target,
       ISNULL(s.Revenue, 0) - COALESCE(t.Target, 0) AS Gap
FROM dbo.Regions r
CROSS JOIN dbo.Months m
LEFT OUTER JOIN dbo.MonthlySales s ON s.Region = r.Region AND s.MonthNumber = m.MonthNumber
FULL OUTER JOIN dbo.Targets t ON t.Region = r.Region AND t.MonthNumber = m.MonthNumber
RIGHT JOIN dbo.ActiveRegions ar ON ar.Region = r.Region
ORDER BY r.Region, m.MonthNumber

-- @batch report_quantified_predicates_and_all_any
SELECT p.Id, p.Name, p.Price
FROM dbo.Products p
WHERE p.Price > ALL (SELECT AVG(Price) FROM dbo.Products GROUP BY CategoryId)
   OR p.Price = ANY (SELECT MAX(Price) FROM dbo.Products)
   OR p.Price < SOME (SELECT MIN(Price) FROM dbo.Products WHERE Discontinued = 1)
ORDER BY p.Price DESC

-- @batch report_string_aggregation_by_hand_and_collate
SELECT c.Name COLLATE Latin1_General_CI_AS AS Name,
       LEN(c.Name) AS NameLength,
       SUBSTRING(c.Email, CHARINDEX('@', c.Email) + 1, 100) AS Domain,
       REPLACE(LTRIM(RTRIM(c.Phone)), ' ', '') AS Phone,
       CURRENT_TIMESTAMP AS ReportedAt,
       @@SERVERNAME AS ServerName,
       DB_NAME() AS DatabaseName
FROM dbo.Customers c
WHERE c.Name COLLATE Latin1_General_BIN = @Name
ORDER BY Name
