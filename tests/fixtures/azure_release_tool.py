#!/usr/bin/env python3
"""Offline Azure/Docker/SWA protocol fixture. Never contacts a network."""
import json
import os
from pathlib import Path
import sys

name = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['TOOL_LOG'], 'a', encoding='utf-8') as stream:
    stream.write(json.dumps([name, *args]) + '\n')
scenario = os.environ.get('FAKE_SCENARIO', '')
image_id = 'sha256:' + 'a' * 64
pinned = 'registry.azurecr.io/sample/api@sha256:' + 'b' * 64

def out(value):
    print(json.dumps(value))
    sys.exit(0)

def option(key):
    return args[args.index(key) + 1]

if name == 'az':
    assert option('--subscription') == 'sub'
    assert '--only-show-errors' in args
    if args[0] == 'storage':
        assert '--account-key' not in args and '--sas-token' not in args
        if args[1] != 'account':
            assert option('--auth-mode') == 'login'
        if args[1:3] == ['account', 'show']:
            out({'allowBlobPublicAccess': scenario == 'public-storage'})
        if args[1:3] == ['container', 'show']:
            assert '--container-name' not in args
            out({'properties': {'publicAccess': None}})
        assert option('--container-name') == 'private'
        if args[2] == 'upload':
            assert option('--overwrite') == 'false' and option('--if-none-match') == '*'
            out({})
        if args[2] == 'download':
            assert option('--overwrite') == 'false'
            Path(option('--file')).write_text('downloaded')
            out({})
        if args[2] == 'lease':
            if args[3] == 'acquire':
                assert option('--lease-duration') == '-1'
                out('fixture-lease')
            assert option('--lease-id') == 'fixture-lease'
            out({})
        if args[2] == 'show':
            out({})
        if args[2] == 'list':
            assert option('--num-results') == '*'
            out([])
    if args[:2] == ['account', 'show']:
        out({'id': 'wrong' if scenario == 'wrong-account' else 'sub', 'tenantId': 'tenant', 'environmentName': 'AzureCloud'})
    if args[0] == 'acr':
        if args[1] == 'show':
            out({'loginServer': 'registry.azurecr.io'})
        if args[1] == 'login':
            print('Login Succeeded')
            sys.exit(0)
        if args[2] == 'list':
            out(['sample/api'] if scenario == 'existing-tag' else [])
        if args[2] == 'show-tags':
            out(['deliver-r1'])
        if args[2] == 'show':
            out({'digest': 'sha256:' + 'b' * 64})
        if args[2] == 'update':
            assert option('--write-enabled') == 'false' and option('--delete-enabled') == 'false'
            out({})
    if args[:3] == ['containerapp', 'revision', 'list']:
        out([{'name': 'app-r1', 'properties': {'active': False, 'replicas': 0, 'template': {'containers': [{'image': pinned}]}, 'secret': 'do-not-retain'}}])
    if args[:3] == ['containerapp', 'job', 'show']:
        out({'properties': {'configuration': {'triggerType': 'Manual'}, 'template': {'containers': [{'image': 'wrong' if scenario == 'wrong-job-image' else pinned}]}}})
    if args[:3] == ['containerapp', 'job', 'start']:
        out({'name': 'execution-123'})
    if args[:4] == ['containerapp', 'job', 'execution', 'list']:
        out([{'name': 'older-success', 'properties': {'status': 'Succeeded'}}, {'name': 'execution-123', 'properties': {'status': 'Failed' if scenario == 'failed-job' else 'Succeeded'}}])
    if args[:2] == ['staticwebapp', 'show']:
        out({'id': '/subscriptions/sub/resourceGroups/' + option('--resource-group') + '/providers/Microsoft.Web/staticSites/' + option('--name')})
    if args[:3] == ['staticwebapp', 'secrets', 'list']:
        out({'properties': {'apiKey': 'private-swa-token'}})
elif name == 'docker':
    if args[:2] == ['image', 'inspect']:
        out([{'Id': 'sha256:' + 'c' * 64 if scenario == 'containerd' else image_id}])
    if args[:2] == ['manifest', 'inspect']:
        out({'config': {'digest': 'wrong' if scenario == 'wrong-registry-image' else image_id}})
    if args[0] in ['load', 'tag', 'push']:
        print('image operation completed')
        sys.exit(0)
elif name == 'swa':
    assert os.environ['SWA_CLI_DEPLOYMENT_TOKEN'] == 'private-swa-token'
    assert '--deployment-token' not in args
    assert '--no-use-keychain' in args
    assert not Path('swa-cli.config.json').exists()
    print('private-swa-token')  # Ensure even unsafe tool output stays private.
    sys.exit(0)
raise AssertionError((name, args))
