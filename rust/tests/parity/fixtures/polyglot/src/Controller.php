<?php
namespace Polyglot;

class Controller
{
    public function handle(string $request): string
    {
        return $this->respond($request);
    }

    protected function respond(string $request): string
    {
        return $request . "-ok";
    }
}

class ApiController extends Controller
{
    public function handle(string $request): string
    {
        return parent::handle($request);
    }
}
