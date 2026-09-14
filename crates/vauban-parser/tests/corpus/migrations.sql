-- Batches in the style of a migration script: IF NOT EXISTS (SELECT * FROM sys.tables …)
-- BEGIN CREATE TABLE … END, ALTER TABLE … ADD CONSTRAINT, CREATE INDEX, a history table,
-- SET QUOTED_IDENTIFIER ON at the head.
-- Written from general knowledge of the shapes such tools produce; no
-- table, column or schema of an identifiable project.
--
-- `SET QUOTED_IDENTIFIER ON` at the head of a batch has NO effect on the lexing of that
-- batch: `ParseOptions` is fixed when `parse_batch` is called, and the parser never
-- re-reads its own statements. No batch below therefore depends on that switch: there is
-- no `"…"` whose meaning would change with it.
--
-- Format: a line `-- @batch <name>` opens a batch, everything up to the next one belongs
-- to it, comments included. This format is local to this corpus.

-- @batch migration_create_history_table
SET QUOTED_IDENTIFIER ON;
IF OBJECT_ID(N'[__MigrationsHistory]') IS NULL
BEGIN
    CREATE TABLE [__MigrationsHistory] (
        [MigrationId] nvarchar(150) NOT NULL,
        [ProductVersion] nvarchar(32) NOT NULL,
        CONSTRAINT [PK___MigrationsHistory] PRIMARY KEY ([MigrationId])
    );
END;

-- @batch migration_create_table_if_not_exists
SET QUOTED_IDENTIFIER ON;
IF NOT EXISTS (SELECT * FROM sys.tables WHERE name = 'Customers' AND schema_id = SCHEMA_ID('dbo'))
BEGIN
    CREATE TABLE [dbo].[Customers] (
        [Id] int IDENTITY(1, 1) NOT NULL,
        [Name] nvarchar(200) NOT NULL,
        [Email] nvarchar(256) NULL,
        [CreatedAt] datetime2(7) NOT NULL DEFAULT (SYSUTCDATETIME()),
        [IsActive] bit NOT NULL CONSTRAINT [DF_Customers_IsActive] DEFAULT ((1)),
        [Balance] decimal(18, 2) NOT NULL DEFAULT 0,
        [RowVersion] rowversion NOT NULL,
        CONSTRAINT [PK_Customers] PRIMARY KEY CLUSTERED ([Id] ASC),
        CONSTRAINT [UQ_Customers_Email] UNIQUE NONCLUSTERED ([Email]),
        CONSTRAINT [CK_Customers_Balance] CHECK ([Balance] >= 0)
    );
END

-- @batch migration_orders_with_fk_and_indexes
CREATE TABLE [dbo].[Orders] (
    [Id] bigint NOT NULL IDENTITY,
    [CustomerId] int NOT NULL,
    [OrderDate] datetime2 NOT NULL,
    [Total] decimal(18, 2) NOT NULL,
    [Status] nvarchar(20) NOT NULL,
    [Notes] nvarchar(max) NULL,
    CONSTRAINT [PK_Orders] PRIMARY KEY ([Id]),
    CONSTRAINT [FK_Orders_Customers_CustomerId] FOREIGN KEY ([CustomerId]) REFERENCES [dbo].[Customers] ([Id]) ON DELETE CASCADE
);
CREATE INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId]);
CREATE INDEX [IX_Orders_OrderDate_Status] ON [dbo].[Orders] ([OrderDate] DESC, [Status]);
CREATE UNIQUE INDEX [IX_Customers_Email] ON [dbo].[Customers] ([Email]) WHERE [Email] IS NOT NULL;

-- @batch migration_add_constraint_separately
ALTER TABLE [dbo].[OrderLines] ADD CONSTRAINT [FK_OrderLines_Orders_OrderId] FOREIGN KEY ([OrderId]) REFERENCES [dbo].[Orders] ([Id]) ON DELETE CASCADE ON UPDATE NO ACTION;
ALTER TABLE [dbo].[OrderLines] ADD CONSTRAINT [CK_OrderLines_Quantity] CHECK ([Quantity] > 0);
ALTER TABLE [dbo].[OrderLines] ADD CONSTRAINT [UQ_OrderLines_Order_Product] UNIQUE ([OrderId], [ProductId]);

-- @batch migration_insert_history_row
INSERT INTO [__MigrationsHistory] ([MigrationId], [ProductVersion])
VALUES (N'20240101120000_InitialCreate', N'8.0.0');

-- @batch migration_add_alter_drop_columns
ALTER TABLE [dbo].[Customers] ADD [Phone] nvarchar(32) NULL;
ALTER TABLE [dbo].[Customers] ADD [Tier] int NOT NULL DEFAULT 0;
ALTER TABLE [dbo].[Customers] DROP CONSTRAINT [DF_Customers_IsActive];
ALTER TABLE [dbo].[Customers] ALTER COLUMN [Name] nvarchar(400) NOT NULL;
ALTER TABLE [dbo].[Customers] DROP COLUMN [Balance];

-- @batch migration_copy_data_in_transaction
BEGIN TRANSACTION;
SELECT [Id], [Name] INTO [dbo].[Customers_Backup] FROM [dbo].[Customers];
INSERT INTO [dbo].[CustomerNames] ([CustomerId], [Name])
SELECT [Id], [Name] FROM [dbo].[Customers] WHERE [Name] IS NOT NULL;
UPDATE [dbo].[Customers] SET [Name] = N'' WHERE [Name] IS NULL;
COMMIT TRANSACTION;

-- @batch migration_down_drop_index_table_history
DROP INDEX [IX_Orders_CustomerId] ON [dbo].[Orders];
DROP TABLE IF EXISTS [dbo].[Customers_Backup];
DROP TABLE [dbo].[OrderLines], [dbo].[Orders];
DELETE FROM [__MigrationsHistory]
WHERE [MigrationId] = N'20240101120000_InitialCreate';

-- @batch migration_drop_unnamed_default_via_dynamic_sql
DECLARE @var0 sysname;
SELECT @var0 = [d].[name]
FROM [sys].[default_constraints] [d]
INNER JOIN [sys].[columns] [c] ON [d].[parent_column_id] = [c].[column_id] AND [d].[parent_object_id] = [c].[object_id]
WHERE ([d].[parent_object_id] = OBJECT_ID(N'[dbo].[Customers]') AND [c].[name] = N'Tier');
IF @var0 IS NOT NULL EXEC(N'ALTER TABLE [dbo].[Customers] DROP CONSTRAINT [' + @var0 + '];');
ALTER TABLE [dbo].[Customers] DROP COLUMN [Tier];

-- @batch migration_create_database_with_options
CREATE DATABASE [AppDb] COLLATE Latin1_General_CI_AS;
ALTER DATABASE [AppDb] SET READ_COMMITTED_SNAPSHOT ON;
ALTER DATABASE [AppDb] SET RECOVERY SIMPLE;

-- @batch migration_use_then_seed
USE [AppDb];
SET NOCOUNT ON;
IF NOT EXISTS (SELECT 1 FROM [dbo].[Roles] WHERE [Name] = N'Administrator')
    INSERT INTO [dbo].[Roles] ([Name], [NormalizedName]) VALUES (N'Administrator', N'ADMINISTRATOR');
IF NOT EXISTS (SELECT 1 FROM [dbo].[Roles] WHERE [Name] = N'User')
    INSERT INTO [dbo].[Roles] ([Name], [NormalizedName]) VALUES (N'User', N'USER');

-- @batch migration_guarded_drop_database
IF EXISTS (SELECT name FROM sys.databases WHERE name = N'AppDb_Test')
BEGIN
    ALTER DATABASE [AppDb_Test] SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE [AppDb_Test];
END
