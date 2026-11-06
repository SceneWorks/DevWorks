"""DW36 revision5 structural metadata and source-configuration wire boundaries; no effects."""
import copy

def append_metadata_cases(case, sample, request, artifact):
    config = sample('ConnectionConfig')
    config.update(name='Shared metadata fixture', engine='MySql', endpoint='mysql://fixture.invalid/catalog',
                  credentialRef='schema-password-handle', catalogs=['catalog'], schemas=['public'],
                  includePatterns=['*'], excludePatterns=['excluded.*'], writesEnabled=False,
                  enabled=True, projectAccess=[dict(projectId='project-a', metadataEnabled=True,
                  dataReadEnabled=True, dataWriteEnabled=False)], workspaceExposed=False,
                  authentication='Password', username='metadata_reader', tls='VerifyFull',
                  caCertificate=copy.deepcopy(artifact), exposureMode='ReadOnly',
                  displayName='Fixture database', environment='Fixture')
    profile = sample('SchemaAdapterProfile')
    profile.update(driver='sqlx-mysql-0.9', serverVersion='8.4', authentication='Password',
                   tls='VerifyFull', readOnlyBoundary='Read-only metadata transaction',
                   supportedMetadata=['Catalog','Schema','Table','Column','Index','Constraint','View','Procedure'],
                   unsupportedMetadata=['Type'], limitations=['Recorded wire fixture only'])
    connection = sample('SchemaConnection')
    connection.update(id='schema-connection', revision='2', name=config['name'], engine='MySql',
                      credentialRef=config['credentialRef'], state='Partial', currentViewId='schema-view',
                      writesEnabled=False, metadataOnly=True, config=copy.deepcopy(config), ownerId='human-a',
                      profile=profile, coverage=dict(state='Partial',gaps=['fixture-coverage-gap'],moreGaps=False),
                      lastSyncedAtMs='1000', lastError='Previous fixture failure', activeOperationId='schema-operation')
    metadata = sample('SchemaObjectMetadata')
    metadata.update(name='parent_id', catalog='catalog', schema='public', comment='Retained metadata',
                    dataType='integer', ordinal='1', nullable=False, primaryKey=False,
                    defaultValue='0', columnKey='MUL', extra='fixture', unique=False,
                    indexType='BTREE', columnNames=['parent_id'], constraintType='FOREIGN KEY',
                    referencedTable='catalog.public.parent', referencedColumns=['id'],
                    checkClause='parent_id >= 0', routineType='PROCEDURE',
                    parameters=[dict(name='input_id',ordinal='1',dataType='integer',mode='IN',nullable=False)],
                    definitionSql='CREATE TABLE child(parent_id INTEGER)')
    obj = sample('SchemaObject')
    obj.update(id='schema-object', viewId='schema-view', kind='Column',
               qualifiedName='catalog.public.child.parent_id', metadata=metadata)
    relation = sample('SchemaRelation')
    relation.update(id='schema-relation', viewId='schema-view', fromObjectId='schema-object',
                    toObjectId='schema-parent', relation='FOREIGN_KEY', inferred=False)
    operations = ['schemas.connections','schemas.objects','schemas.relationships','schemas.index',
                  'schemas.refresh','schemas.remove_index','schemas.configure','database.configure_sensitive_columns']
    for name in operations:
        mode = 'Query' if name in operations[:3] else 'Command'
        frame = request(name,mode)
        frame['schemaRevision'] = 5
        body = frame['body']
        if 'connectionId' in body: body['connectionId'] = 'schema-connection'
        if 'views' in body: body['views'] = dict(kind='SchemaPublished',connectionIds=['schema-connection'])
        if name == 'schemas.configure': body.update(config=copy.deepcopy(config),action='Save')
        if name in ['schemas.index','schemas.refresh']: body['testOnly'] = True
        case('dw36-r5-admission-'+name,'domain-request.r5',frame)
        if name == 'schemas.configure' or name in ['schemas.index','schemas.refresh']:
            before = copy.deepcopy(frame); before['schemaRevision'] = 1
            case('dw36-before5-'+name,'domain-request',before,False,False)
        if name == 'schemas.connections': value=dict(items=[connection],hasMore=False)
        elif name == 'schemas.objects': value=dict(items=[obj],hasMore=False)
        elif name == 'schemas.relationships': value=dict(items=[relation],hasMore=False)
        elif name in ['schemas.configure','database.configure_sensitive_columns']: value=connection
        elif name == 'schemas.remove_index': value=sample('MutationSummary')
        else: value=sample('IndexRunRecord')
        output=dict(kind=name,value=copy.deepcopy(value))
        case('dw36-r5-result-'+name,'operation-result.r5',output)
        if name in ['schemas.connections','schemas.objects','schemas.configure']:
            case('dw36-before5-result-'+name,'operation-result.r4',output,False,False)
    for engine in ['Sqlite','MariaDb','MySql','Postgres','SqlServer']:
        frame=request('schemas.configure','Command');frame['schemaRevision']=5
        configured=copy.deepcopy(config);configured['engine']=engine
        if engine=='Sqlite':
            configured.update(endpoint='/fixture/metadata.sqlite',authentication='File')
            for key in ['credentialRef','username','tls','caCertificate']:configured.pop(key,None)
        frame['body'].update(config=configured,action='Save')
        case('dw36-r5-engine-'+engine,'domain-request.r5',frame)
    for field in ['enabled','projectAccess','authentication','tls','exposureMode']:
        frame=request('schemas.configure','Command');frame['schemaRevision']=5
        frame['body']['config']=copy.deepcopy(config);frame['body']['config'][field]=None
        case('dw36-null-'+field,'domain-request.r5',frame,False,False)

    for name in ['schemas.index','schemas.refresh']:
        scoped=request(name,'Command');scoped['schemaRevision']=5
        scoped['body']['scope']=dict(kind='Project',projectId='project-a')
        case('dw36-r5-project-admission-'+name,'domain-request.r5',scoped)
        old=copy.deepcopy(scoped);old['schemaRevision']=1
        case('dw36-before5-project-'+name,'domain-request',old,False,False)

    exact=request('schemas.objects','Query');exact['schemaRevision']=5;exact['body']['objectId']='exact-schema-object'
    case('dw36-r5-exact-object','domain-request.r5',exact)
    old=copy.deepcopy(exact);old['schemaRevision']=1
    case('dw36-before5-exact-object','domain-request',old,False,False)
