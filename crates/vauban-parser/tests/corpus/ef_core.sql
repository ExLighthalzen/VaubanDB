-- Batches in the style of what Entity Framework Core sends: every name in brackets,
-- aliases [t], [t0], [o], [c], parameters @__x_0 / @p0, booleans as CAST(1 AS bit),
-- paging through ORDER BY … OFFSET/FETCH, SaveChanges as SET NOCOUNT ON + DML + SELECT.
-- Written from general knowledge of the shapes this tool produces; no
-- table, column or schema of an identifiable project.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch ef_simple_paged_query
SELECT [t].[Id], [t].[Name], [t].[CreatedAt], [t].[IsActive]
FROM [Customers] AS [t]
WHERE [t].[IsActive] = CAST(1 AS bit)
ORDER BY [t].[Name], [t].[Id]
OFFSET @__p_0 ROWS FETCH NEXT @__p_1 ROWS ONLY

-- @batch ef_left_join_cascade
SELECT [o].[Id], [o].[OrderDate], [o].[CustomerId], [c].[Id], [c].[Name], [c].[Email], [l].[Id], [l].[OrderId], [l].[ProductId], [l].[Quantity], [l].[UnitPrice], [p].[Id], [p].[Name], [p].[Sku]
FROM [Orders] AS [o]
INNER JOIN [Customers] AS [c] ON [o].[CustomerId] = [c].[Id]
LEFT JOIN [OrderLines] AS [l] ON [o].[Id] = [l].[OrderId]
LEFT JOIN [Products] AS [p] ON [l].[ProductId] = [p].[Id]
WHERE [o].[OrderDate] >= @__start_0 AND [o].[OrderDate] < @__end_1
ORDER BY [o].[Id], [c].[Id], [l].[Id]

-- @batch ef_exists_correlated
SELECT [c].[Id], [c].[Name]
FROM [Customers] AS [c]
WHERE EXISTS (
    SELECT 1
    FROM [Orders] AS [o]
    WHERE [c].[Id] = [o].[CustomerId] AND [o].[Total] > 100.0)

-- @batch ef_in_subquery_and_case_bit
SELECT [t].[Id], CASE
    WHEN [t].[DeletedAt] IS NULL THEN CAST(1 AS bit)
    ELSE CAST(0 AS bit)
END AS [IsAlive], [t].[Name]
FROM [Tenants] AS [t]
WHERE [t].[Id] IN (
    SELECT [m].[TenantId]
    FROM [Memberships] AS [m]
    WHERE [m].[UserId] = @__userId_0
)

-- @batch ef_any_as_case_exists
SELECT CASE
    WHEN EXISTS (
        SELECT 1
        FROM [Users] AS [u]
        WHERE [u].[NormalizedEmail] = @__normalizedEmail_0) THEN CAST(1 AS bit)
    ELSE CAST(0 AS bit)
END

-- @batch ef_first_or_default_top1
SELECT TOP(1) [u].[Id], [u].[Email], [u].[NormalizedEmail], [u].[PasswordHash], [u].[SecurityStamp], [u].[LockoutEnd], [u].[AccessFailedCount]
FROM [Users] AS [u]
WHERE [u].[NormalizedEmail] = @__normalizedEmail_0

-- @batch ef_count_star_with_where
SELECT COUNT(*)
FROM [Orders] AS [o]
WHERE [o].[Status] = @__status_0 AND [o].[CustomerId] = @__customerId_1

-- @batch ef_savechanges_insert
SET NOCOUNT ON;
INSERT INTO [Customers] ([Name], [Email], [CreatedAt], [IsActive])
VALUES (@p0, @p1, @p2, @p3);
SELECT [Id]
FROM [Customers]
WHERE @@ROWCOUNT = 1 AND [Id] = scope_identity();

-- @batch ef_savechanges_update_and_delete
SET NOCOUNT ON;
UPDATE [Customers] SET [Name] = @p0, [IsActive] = @p1
WHERE [Id] = @p2;
SELECT @@ROWCOUNT;
DELETE FROM [Orders]
WHERE [Id] = @p3;
SELECT @@ROWCOUNT;

-- @batch ef_group_by_key_count_sum
SELECT [o].[CustomerId] AS [Key], COUNT(*) AS [Count], SUM([o].[Total]) AS [Total], MAX([o].[OrderDate]) AS [Last]
FROM [Orders] AS [o]
GROUP BY [o].[CustomerId]
HAVING COUNT(*) > 1
ORDER BY [o].[CustomerId]

-- @batch ef_string_functions_order_by_select_1
SELECT [p].[Id], [p].[Name], [p].[Price], [p].[Discount]
FROM [Products] AS [p]
WHERE ([p].[Name] LIKE N'%widget%') AND (LEFT([p].[Sku], 3) = N'WID') AND ([p].[Price] IS NOT NULL) AND (COALESCE([p].[Discount], 0.0) < 0.5) AND (LOWER([p].[Name]) <> UPPER([p].[Name]))
ORDER BY (SELECT 1)
OFFSET @__p_0 ROWS FETCH NEXT @__p_1 ROWS ONLY

-- @batch ef_union_in_derived_table
SELECT [t].[Id], [t].[Name], [t].[Kind]
FROM (
    SELECT [c].[Id], [c].[Name], N'Customer' AS [Kind]
    FROM [Customers] AS [c]
    WHERE [c].[IsActive] = CAST(1 AS bit)
    UNION
    SELECT [s].[Id], [s].[Name], N'Supplier' AS [Kind]
    FROM [Suppliers] AS [s]
    WHERE [s].[IsActive] = CAST(1 AS bit)
) AS [t]
ORDER BY [t].[Name]

-- @batch ef_null_semantics_and_datetime_parts
SELECT [o].[Id], [o].[OrderDate]
FROM [Orders] AS [o]
WHERE (([o].[ShippedAt] IS NULL) OR ([o].[ShippedAt] > [o].[OrderDate])) AND (DATEPART(year, [o].[OrderDate]) = @__year_0) AND ([o].[Note] IS NULL OR [o].[Note] <> N'')
ORDER BY [o].[OrderDate] DESC, [o].[Id]

-- @batch ef_execute_delete_bulk
DELETE FROM [n]
FROM [Notifications] AS [n]
WHERE [n].[ReadAt] IS NOT NULL AND [n].[ReadAt] < DATEADD(day, CAST(-30.0E0 AS int), GETUTCDATE())

-- @batch ef_execute_update_bulk
UPDATE [p]
SET [p].[Price] = [p].[Price] * 1.1, [p].[UpdatedAt] = GETUTCDATE()
FROM [Products] AS [p]
INNER JOIN [Categories] AS [c] ON [p].[CategoryId] = [c].[Id]
WHERE [c].[Name] = @__name_0
